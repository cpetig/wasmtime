use anyhow::{Context, Result};
use std::path::Path;
use std::str::FromStr;

use crate::cases::Case;
use crate::compile;

#[derive(Debug)]
pub enum CaseOutcome {
    Pass,
    Fail(String),
    Skip(String),
}

impl CaseOutcome {
    pub fn is_pass(&self) -> bool {
        matches!(self, CaseOutcome::Pass)
    }

    pub fn is_fail(&self) -> bool {
        matches!(self, CaseOutcome::Fail(_))
    }

    pub fn is_skip(&self) -> bool {
        matches!(self, CaseOutcome::Skip(_))
    }
}

pub fn run_case_single(case: &Case, workdir: &Path) -> Result<CaseOutcome> {
    // Step 1: Build compiler
    let (compiler, tunables) = compile::build_arm32_compiler()?;

    // Step 2: Compile the wasm module's function
    let (bytes, alignment, translation) =
        match compile_wasm_function(&*compiler, &tunables, &case.module, &case.export) {
            Ok(result) => result,
            Err(e) => {
                // Check if this is an unsupported operation
                let err_str = e.to_string();
                if err_str.contains("Unsupported feature")
                    || err_str.contains("should be implemented in ISLE")
                {
                    return Ok(CaseOutcome::Skip(err_str));
                }
                return Ok(CaseOutcome::Fail(err_str));
            }
        };

    // Step 3: Emit object file with wasm function and trampoline
    let obj_bytes = emit_object_file_with_trampoline(
        &bytes,
        alignment,
        &translation,
        &case.export,
        case.args.len(),
    )?;

    // Write object file
    let obj_path = workdir.join("module.o");
    std::fs::write(&obj_path, &obj_bytes)?;

    // Step 4: Generate and write C driver
    let offsets = get_vmctx_offsets(&translation);
    let c_source = generate_c_driver(
        offsets.store_ctx_off,
        offsets.stack_limit_off,
        &case.args,
        case.expected,
    );
    let driver_path = workdir.join("driver.c");
    std::fs::write(&driver_path, &c_source)?;

    // Step 5: Link
    let elf_path = workdir.join("program");
    link_with_gcc(&obj_path, &driver_path, &elf_path)?;

    // Step 6: Run under QEMU
    let got = run_under_qemu(&elf_path)?;

    // Step 7: Compare result
    if got == case.expected {
        Ok(CaseOutcome::Pass)
    } else {
        Ok(CaseOutcome::Fail(format!(
            "expected {}, got {}",
            case.expected, got
        )))
    }
}

fn compile_wasm_function<'data>(
    compiler: &dyn wasmtime_environ::Compiler,
    tunables: &wasmtime_environ::Tunables,
    wasm_bytes: &'data [u8],
    export_name: &str,
) -> Result<(Vec<u8>, u32, wasmtime_environ::ModuleTranslation<'data>)> {
    use wasmparser::{Parser, Validator};
    use wasmtime_environ::{FuncKey, ModuleEnvironment, ModuleTypesBuilder, StaticModuleIndex};

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

    // Find the function index for the export
    let func_index = find_export_func_index(&translation.module, export_name)
        .ok_or_else(|| anyhow::anyhow!("export '{}' not found", export_name))?;

    // Move function bodies out - we need to clone the map but FunctionBodyData doesn't Clone
    // So we use a workaround: iterate and collect the specific body we need
    let bodies = std::mem::take(&mut translation.function_body_inputs);

    // Find the body for our function index by iterating
    let body_data = bodies
        .into_iter()
        .find(|(idx, _)| *idx == func_index)
        .map(|(_, data)| data)
        .ok_or_else(|| anyhow::anyhow!("function body not found for index {:?}", func_index))?;

    let key = FuncKey::DefinedWasmFunction(StaticModuleIndex::from_u32(0), func_index);

    // Compile the function
    let mut cfb = compiler.compile_function(&translation, key, body_data, &types, export_name)?;

    // Finish compiling
    compiler
        .inlining_compiler()
        .ok_or_else(|| anyhow::anyhow!("compiler does not support inlining"))?
        .finish_compiling(&mut cfb, None, export_name)
        .map_err(|e| anyhow::anyhow!("failed to finish compiling: {}", e))?;

    // Extract machine code bytes
    let cf = cfb
        .code
        .downcast_ref::<wasmtime_cranelift::CompiledFunction>()
        .ok_or_else(|| anyhow::anyhow!("expected CompiledFunction"))?;

    let bytes = cf.buffer.data().to_vec();
    let alignment = cf.alignment;

    Ok((bytes, alignment, translation))
}

fn find_export_func_index(
    module: &wasmtime_environ::Module,
    name: &str,
) -> Option<wasmtime_environ::DefinedFuncIndex> {
    use wasmtime_environ::EntityIndex;

    for (atom, entity_idx) in &module.exports {
        let atom_str = module.strings.get(*atom);
        if atom_str == Some(name) {
            match entity_idx {
                EntityIndex::Function(idx) => {
                    // FuncIndex is a u32 wrapper, we need to convert to DefinedFuncIndex
                    // For now just use the raw value - this works because both are u32-based
                    return Some(wasmtime_environ::DefinedFuncIndex::from_u32(idx.as_u32()));
                }
                _ => continue,
            }
        }
    }
    None
}

fn emit_object_file_with_trampoline(
    bytes: &[u8],
    alignment: u32,
    translation: &wasmtime_environ::ModuleTranslation<'_>,
    export_name: &str,
    arity: usize,
) -> Result<Vec<u8>> {
    use cranelift_codegen::ir::{AbiParam, Signature, types};
    use cranelift_codegen::isa::{CallConv, lookup};
    use cranelift_codegen::settings;
    use cranelift_module::{Linkage, Module, default_libcall_names};
    use cranelift_object::{ObjectBuilder, ObjectModule};

    let triple_str = "thumbv7em-none-eabihf";
    let triple = target_lexicon::Triple::from_str(triple_str)
        .map_err(|e| anyhow::anyhow!("failed to parse target triple: {}", e))?;
    let isa = lookup(triple.clone())?.finish(settings::Flags::new(settings::builder()))?;
    let frontend_config = isa.frontend_config();
    let builder = ObjectBuilder::new(isa, "module", default_libcall_names())?;
    let mut module = ObjectModule::new(builder);

    let ptr = types::I32;

    // Declare wasm function with Tail calling convention
    let mut wasm_sig = Signature::new(CallConv::Tail);
    wasm_sig.params.push(AbiParam::new(ptr)); // vmctx
    wasm_sig.params.push(AbiParam::new(ptr)); // caller_vmctx
    for _ in 0..arity {
        wasm_sig.params.push(AbiParam::new(types::I32));
    }
    // For i32-only functions, we assume single i32 return (if any)
    wasm_sig.returns.push(AbiParam::new(types::I32));

    let wasm_id = module.declare_function(export_name, Linkage::Local, &wasm_sig)?;

    // Define the wasm function from compiled bytes
    module.define_function_bytes(wasm_id, alignment as u64, bytes, &[])?;

    // Build trampoline with AAPCS calling convention
    let mut entry_sig = Signature::new(CallConv::triple_default(&triple));
    entry_sig.params.push(AbiParam::new(ptr)); // vmctx
    for _ in 0..arity {
        entry_sig.params.push(AbiParam::new(types::I32));
    }
    // For i32-only functions, we assume single i32 return (if any)
    entry_sig.returns.push(AbiParam::new(types::I32));

    let entry_id = module.declare_function("test_entry", Linkage::Export, &entry_sig)?;

    // Build trampoline function
    use cranelift_codegen::Context;
    use cranelift_codegen::ir::{Function, InstBuilder, UserFuncName};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    let mut func =
        Function::with_name_signature(UserFuncName::user(0, entry_id.as_u32()), entry_sig.clone());

    // Import wasm function into trampoline
    let callee = module.declare_func_in_func(wasm_id, &mut func);

    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fctx);
    let blk = b.create_block();
    b.append_block_params_for_function_params(blk);
    b.switch_to_block(blk);
    b.seal_block(blk);

    // Get parameters: r0=vmctx, r1..rN=args
    let vmctx = b.block_params(blk)[0];
    let args: Vec<_> = b.block_params(blk)[1..].to_vec();

    // Call wasm function with Tail convention: (vmctx, caller_vmctx, arg0, ...)
    let mut call_args = vec![vmctx, vmctx];
    call_args.extend(args);
    let call = b.ins().call(callee, &call_args);
    let res = b.inst_results(call)[0];
    b.ins().return_(&[res]);
    b.finalize(frontend_config);

    let mut ctx = Context::new();
    ctx.func = func;

    module.define_function(entry_id, &mut ctx)?;

    Ok(module.finish().emit()?)
}

fn get_vmctx_offsets(translation: &wasmtime_environ::ModuleTranslation<'_>) -> VmctxOffsets {
    use wasmtime_environ::{PtrSize, VMOffsets};

    let offsets = VMOffsets::new(4u8, &translation.module);
    VmctxOffsets {
        store_ctx_off: offsets.ptr.vmcontext_store_context() as u32,
        stack_limit_off: offsets.ptr.vmstore_context_stack_limit() as u32,
    }
}

struct VmctxOffsets {
    store_ctx_off: u32,
    stack_limit_off: u32,
}

fn generate_c_driver(
    store_ctx_off: u32,
    stack_limit_off: u32,
    args: &[i32],
    expected: i32,
) -> String {
    let sc_base = 64u32;
    let num_args = args.len();

    let args_params = if num_args > 0 {
        args.iter()
            .enumerate()
            .map(|(i, _)| format!(" int a{}", i))
            .collect::<Vec<_>>()
            .join(",")
    } else {
        String::new()
    };

    let args_call = if num_args > 0 {
        args.iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    };

    let args_params_str = if num_args > 0 {
        format!(", {}", args_params)
    } else {
        String::new()
    };

    let args_call_str = if num_args > 0 {
        format!(", {}", args_call)
    } else {
        String::new()
    };

    format!(
        r#"#include <stdint.h>
#include <stdio.h>

extern int test_entry(void *vmctx{args_params});

int main(void) {{
    unsigned char buf[256] = {{0}};
    // VMContext.store_context (at STORE_CTX_OFF) -> points at buf+SC_BASE
    *(uintptr_t*)(buf + {store_ctx_off}) = (uintptr_t)(buf + {sc_base});
    // VMStoreContext.stack_limit at SC_BASE + STACK_LIMIT_OFF is 0 (buf is zero-initialized)
    int got = test_entry((void*)buf{args_call});
    printf("%d\n", got);
    return got == {expected} ? 0 : 1;
}}
"#,
        store_ctx_off = store_ctx_off,
        sc_base = sc_base,
        args_params = args_params_str,
        args_call = args_call_str
    )
}

fn link_with_gcc(obj_path: &Path, driver_path: &Path, elf_path: &Path) -> Result<()> {
    let status = std::process::Command::new("arm-linux-gnueabihf-gcc")
        .arg("-mthumb")
        .arg(driver_path)
        .arg(obj_path)
        .arg("-o")
        .arg(elf_path)
        .arg("-L")
        .arg("/usr/arm-linux-gnueabihf/lib")
        .status()?;

    if !status.success() {
        anyhow::bail!("linking failed with status: {}", status);
    }
    Ok(())
}

fn run_under_qemu(elf_path: &Path) -> Result<i32> {
    let output = std::process::Command::new("qemu-arm-static")
        .arg("-L")
        .arg("/usr/arm-linux-gnueabihf")
        .arg(elf_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "QEMU execution failed with status: {}, stderr: {}",
            output.status,
            stderr.trim()
        );
    }

    let result = String::from_utf8_lossy(&output.stdout);
    let got: i32 = result
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("failed to parse QEMU output as i32: {}", result))?;

    Ok(got)
}
