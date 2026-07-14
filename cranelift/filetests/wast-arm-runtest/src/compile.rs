use anyhow::Result;
use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName, types};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, lookup};
use cranelift_codegen::{Context, settings};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::str::FromStr;
use target_lexicon::{PointerWidth, Triple};
use wasmtime_cranelift::builder;
use wasmtime_environ::{Compiler, FuncKey, StaticModuleIndex, Tunables};

pub fn build_arm32_compiler() -> Result<(Box<dyn Compiler>, Tunables)> {
    let triple = Triple::from_str("thumbv7em-none-eabihf")
        .map_err(|e| anyhow::anyhow!("failed to parse target triple: {}", e))?;
    let tunables = Tunables::default_for_target(&triple)
        .map_err(|e| anyhow::anyhow!("failed to create default tunables for target: {}", e))?;
    let mut b = builder(Some(triple))
        .map_err(|e| anyhow::anyhow!("failed to create compiler builder: {}", e))?;
    b.set_tunables(tunables.clone())
        .map_err(|e| anyhow::anyhow!("failed to set tunables: {}", e))?;
    let compiler = b
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build compiler: {}", e))?;
    Ok((compiler, tunables))
}

/// Compile a wasm module's first function and extract the machine code
/// Returns: (bytes, alignment, module_translation)
pub fn compile_wasm_function<'data>(
    compiler: &dyn Compiler,
    tunables: &Tunables,
    wasm_bytes: &'data [u8],
) -> Result<(Vec<u8>, u32, wasmtime_environ::ModuleTranslation<'data>)> {
    use wasmparser::{Parser, Validator};
    use wasmtime_environ::{ModuleEnvironment, ModuleTypesBuilder};

    let mut validator = Validator::new();
    let mut types = ModuleTypesBuilder::new(&validator);
    let env = ModuleEnvironment::new(
        tunables,
        &mut validator,
        &mut types,
        StaticModuleIndex::from_u32(0),
    );

    let mut translation = env
        .translate(Parser::new(0), wasm_bytes)
        .map_err(|e| anyhow::anyhow!("failed to translate module: {}", e))?;

    // Move function bodies out
    let bodies = std::mem::take(&mut translation.function_body_inputs);
    let (def_index, body_data) = bodies
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no defined functions in module"))?;

    let key = FuncKey::DefinedWasmFunction(StaticModuleIndex::from_u32(0), def_index);

    // Compile the function - this returns CompiledFunctionBody with CompilerContext
    let mut cfb = compiler
        .compile_function(&translation, key, body_data, &types, "wasm_add")
        .map_err(|e| anyhow::anyhow!("failed to compile function: {}", e))?;

    // finish_compiling converts CompilerContext to CompiledFunction
    // Pass None for input since we already have the compiled context
    let symbol = "wasm_add";
    compiler
        .inlining_compiler()
        .ok_or_else(|| anyhow::anyhow!("compiler does not support inlining"))?
        .finish_compiling(&mut cfb, None, symbol)
        .map_err(|e| anyhow::anyhow!("failed to finish compiling: {}", e))?;

    // Now we can downcast to CompiledFunction
    let cf = cfb
        .code
        .downcast_ref::<wasmtime_cranelift::CompiledFunction>()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expected CompiledFunction, got {}",
                std::any::type_name_of_val(&cfb.code)
            )
        })?;

    // Extract the machine code bytes from the buffer
    let bytes = cf.buffer.data().to_vec();
    let alignment = cf.alignment;

    Ok((bytes, alignment, translation))
}

/// Emit a linkable object file with the wasm function symbol and trampoline
pub fn emit_object_file(
    bytes: &[u8],
    alignment: u32,
    translation: &wasmtime_environ::ModuleTranslation<'_>,
) -> Result<Vec<u8>> {
    let triple_str = "thumbv7em-none-eabihf";
    let triple = Triple::from_str(triple_str)
        .map_err(|e| anyhow::anyhow!("failed to parse target triple: {}", e))?;
    let isa = lookup(triple.clone())?.finish(settings::Flags::new(settings::builder()))?;
    let builder = ObjectBuilder::new(isa, "spike", default_libcall_names())?;
    let mut module = ObjectModule::new(builder);

    let ptr = types::I32;

    // Declare wasm function with Tail calling convention (as per Background fact #2)
    // Signature: (vmctx, caller_vmctx, arg0, arg1) -> i32
    let mut wasm_sig = Signature::new(CallConv::Tail);
    wasm_sig.params.push(AbiParam::new(ptr)); // vmctx
    wasm_sig.params.push(AbiParam::new(ptr)); // caller_vmctx
    wasm_sig.params.push(AbiParam::new(types::I32)); // a0
    wasm_sig.params.push(AbiParam::new(types::I32)); // a1
    wasm_sig.returns.push(AbiParam::new(types::I32));

    let wasm_id = module.declare_function("wasm_add", Linkage::Local, &wasm_sig)?;

    // Declare trampoline function with AAPCS calling convention
    // This will be the exported function that C can call
    // Signature: (vmctx, arg0, arg1) -> i32
    let mut entry_sig = Signature::new(CallConv::triple_default(&triple));
    entry_sig.params.push(AbiParam::new(ptr)); // vmctx
    entry_sig.params.push(AbiParam::new(types::I32)); // a0
    entry_sig.params.push(AbiParam::new(types::I32)); // a1
    entry_sig.returns.push(AbiParam::new(types::I32));

    let entry_id = module.declare_function("test_entry", Linkage::Export, &entry_sig)?;

    // Build trampoline function that calls wasm_add
    // The wasm function is defined first with the compiled bytes
    module.define_function_bytes(wasm_id, alignment as u64, bytes, &[])?;

    let mut func =
        Function::with_name_signature(UserFuncName::user(0, entry_id.as_u32()), entry_sig.clone());

    // Import wasm_add into this function BEFORE building the body
    let callee = module.declare_func_in_func(wasm_id, &mut func);

    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fctx);
    let blk = b.create_block();
    b.append_block_params_for_function_params(blk);
    b.switch_to_block(blk);
    b.seal_block(blk);

    // In AAPCS: r0=vmctx, r1=a0, r2=a1
    let vmctx = b.block_params(blk)[0];
    let a0 = b.block_params(blk)[1];
    let a1 = b.block_params(blk)[2];

    // Tail callee expects (vmctx, caller_vmctx, arg0, arg1); pass vmctx twice
    let call = b.ins().call(callee, &[vmctx, vmctx, a0, a1]);
    let res = b.inst_results(call)[0];
    b.ins().return_(&[res]);
    b.finalize(TargetFrontendConfig {
        default_call_conv: CallConv::triple_default(&triple),
        pointer_width: PointerWidth::U32,
        page_size_align_log2: 12,
    });

    let mut ctx = Context::new();
    ctx.func = func;

    module.define_function(entry_id, &mut ctx)?;

    let obj_bytes = module.finish().emit()?;
    Ok(obj_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::isa::CallConv;

    #[test]
    fn test_object_emission() {
        let triple = Triple::from_str("thumbv7em-none-eabihf").unwrap();
        let isa = lookup(triple.clone())
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let mut module = ObjectModule::new(
            ObjectBuilder::new(isa.clone(), "test", default_libcall_names()).unwrap(),
        );

        // Declare func_a
        let mut sig_a = Signature::new(CallConv::triple_default(&triple));
        sig_a.params.push(AbiParam::new(types::I32));
        sig_a.returns.push(AbiParam::new(types::I32));

        let func_a_id = module
            .declare_function("func_a", Linkage::Local, &sig_a)
            .unwrap();

        let mut ctx_a = Context::new();
        ctx_a.func =
            Function::with_name_signature(UserFuncName::user(0, func_a_id.as_u32()), sig_a.clone());
        let mut func_ctx = FunctionBuilderContext::new();
        {
            let mut bcx = FunctionBuilder::new(&mut ctx_a.func, &mut func_ctx);
            let block = bcx.create_block();
            bcx.switch_to_block(block);
            bcx.append_block_params_for_function_params(block);
            let param = bcx.block_params(block)[0];
            let result = bcx.ins().iadd_imm(param, 1);
            bcx.ins().return_(&[result]);
            bcx.seal_all_blocks();
            bcx.finalize(isa.frontend_config());
        }
        module.define_function(func_a_id, &mut ctx_a).unwrap();

        // Declare func_b
        let mut sig_b = Signature::new(CallConv::triple_default(&triple));
        sig_b.params.push(AbiParam::new(types::I32));
        sig_b.returns.push(AbiParam::new(types::I32));

        let func_b_id = module
            .declare_function("func_b", Linkage::Local, &sig_b)
            .unwrap();

        let mut ctx_b = Context::new();
        ctx_b.func =
            Function::with_name_signature(UserFuncName::user(0, func_b_id.as_u32()), sig_b.clone());
        {
            let mut bcx = FunctionBuilder::new(&mut ctx_b.func, &mut func_ctx);
            let block = bcx.create_block();
            bcx.switch_to_block(block);
            bcx.append_block_params_for_function_params(block);
            let param = bcx.block_params(block)[0];
            let local_func = module.declare_func_in_func(func_a_id, &mut bcx.func);
            let result = bcx.ins().call(local_func, &[param]);
            let return_val = bcx.inst_results(result)[0];
            bcx.ins().return_(&[return_val]);
            bcx.seal_all_blocks();
            bcx.finalize(isa.frontend_config());
        }
        module.define_function(func_b_id, &mut ctx_b).unwrap();

        let obj_bytes = module.finish().emit().unwrap();
        std::fs::write("/tmp/test_object.o", &obj_bytes).unwrap();
    }
}

/// Generate C driver source code for testing the compiled wasm function
pub fn generate_c_driver(store_ctx_off: u32, stack_limit_off: u32) -> String {
    let sc_base = 64u32;

    format!(
        r#"#include <stdint.h>
#include <stdio.h>

extern int test_entry(void *vmctx, int a, int b);

int main(void) {{
    unsigned char buf[256] = {{0}};
    // VMContext.store_context (at STORE_CTX_OFF) -> points at buf+SC_BASE
    *(uintptr_t*)(buf + {store_ctx_off}) = (uintptr_t)(buf + {sc_base});
    // VMStoreContext.stack_limit at SC_BASE + STACK_LIMIT_OFF is 0 (buf is zero-initialized)
    int got = test_entry((void*)buf, 2, 3);
    printf("%d\n", got);
    return got == 5 ? 0 : 1;
}}
"#,
        store_ctx_off = store_ctx_off,
        sc_base = sc_base
    )
}
