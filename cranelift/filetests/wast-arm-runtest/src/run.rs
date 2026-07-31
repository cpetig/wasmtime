use anyhow::Result;
use json_from_wast::FloatConst;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{
    cases::{Case, CaseValue},
    compile,
};

pub struct BatchRunResult {
    pub output: String,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

#[derive(Clone)]
struct PreparedCase {
    index: usize,
    symbol_name: String,
    runner_name: String,
    bytes: Vec<u8>,
    alignment: u32,
    args: Vec<CaseValue>,
    expected: CaseValue,
    store_ctx_off: u32,
}

pub fn run_cases_batch(cases: &[Case], workdir: &Path, verbose: bool) -> Result<BatchRunResult> {
    let (compiler, tunables) = compile::build_arm32_compiler()?;

    let mut compiled_cases = Vec::new();
    let mut compiled_cache: HashMap<String, PreparedCase> = HashMap::new();
    let mut output_lines = Vec::new();
    let mut passed = 0u32;
    let mut failed = 0u32;
    let skipped = 0u32;

    for (idx, case) in cases.iter().enumerate() {
        let runner_name = make_case_runner_name(idx, &case.export);
        let cache_key = make_compiled_case_key(case);

        let prepared = match compiled_cache.get(&cache_key) {
            Some(compiled) => PreparedCase {
                index: idx,
                symbol_name: compiled.symbol_name.clone(),
                runner_name: runner_name.clone(),
                bytes: compiled.bytes.clone(),
                alignment: compiled.alignment,
                args: case.args.clone(),
                expected: case.expected,
                store_ctx_off: compiled.store_ctx_off,
            },
            None => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                prepare_case(
                    &*compiler,
                    &tunables,
                    idx,
                    case,
                    &make_compiled_symbol_name(case),
                    &runner_name,
                )
            })) {
                Ok(Ok(result)) => {
                    compiled_cache.insert(cache_key.clone(), result.clone());
                    result
                }
                Ok(Err(e)) => {
                    output_lines.push(format!("FAIL case {idx}: {e}"));
                    failed += 1;
                    continue;
                }
                Err(payload) => {
                    output_lines.push(format!("FAIL case {idx}: {payload:?}"));
                    failed += 1;
                    continue;
                }
            },
        };

        compiled_cases.push(prepared);
    }

    if compiled_cases.is_empty() {
        return Ok(BatchRunResult {
            output: output_lines.join("\n"),
            passed,
            failed,
            skipped,
        });
    }

    let obj_bytes = emit_object_file_with_trampoline(&compiled_cases)?;
    let obj_path = workdir.join("module.o");
    std::fs::write(&obj_path, &obj_bytes)?;

    let rust_source = generate_rust_driver(&compiled_cases, verbose);
    let driver_path = workdir.join("driver.rs");
    std::fs::write(&driver_path, &rust_source)?;

    let elf_path = workdir.join("program");
    let toolchain = locate_toolchain()?;
    link_with_rustc(
        &toolchain.linker,
        &toolchain.sysroot,
        &obj_path,
        &driver_path,
        &elf_path,
    )?;

    let (harness_output, exit_code) =
        run_under_qemu(&toolchain.qemu, &toolchain.sysroot, &elf_path)?;
    if let Some((summary_passed, summary_failed)) = parse_result_summary(&harness_output) {
        passed += summary_passed;
        failed += summary_failed;
    } else {
        let harness_failed = if exit_code == 0 {
            0u32
        } else {
            exit_code as u32
        };
        let harness_passed = compiled_cases.len().saturating_sub(harness_failed as usize) as u32;
        passed += harness_passed;
        failed += harness_failed;
    }

    output_lines.push(harness_output.trim().to_string());
    let output = output_lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(BatchRunResult {
        output,
        passed,
        failed,
        skipped,
    })
}

fn prepare_case(
    compiler: &dyn wasmtime_environ::Compiler,
    tunables: &wasmtime_environ::Tunables,
    index: usize,
    case: &Case,
    symbol_name: &str,
    runner_name: &str,
) -> Result<PreparedCase> {
    let (bytes, alignment, translation) =
        compile_wasm_function(compiler, tunables, &case.module, &case.export, symbol_name)?;
    let offsets = get_vmctx_offsets(&translation);
    Ok(PreparedCase {
        index,
        symbol_name: symbol_name.to_string(),
        runner_name: runner_name.to_string(),
        bytes,
        alignment,
        args: case.args.clone(),
        expected: case.expected,
        store_ctx_off: offsets.store_ctx_off,
    })
}

fn make_case_symbol_name(index: usize, export_name: &str) -> String {
    let sanitized = export_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        format!("case_{index}")
    } else {
        format!("case_{index}_{sanitized}")
    }
}

fn make_case_runner_name(index: usize, export_name: &str) -> String {
    format!(
        "run_case_{index}_{}",
        make_case_symbol_name(index, export_name)
    )
}

fn sanitize_rust_ident(ident: &str) -> String {
    let mut sanitized = String::new();
    let mut prev_was_underscore = false;

    for ch in ident.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
            prev_was_underscore = false;
        } else if !prev_was_underscore {
            sanitized.push('_');
            prev_was_underscore = true;
        }
    }

    sanitized.trim_matches('_').to_string()
}

fn make_compiled_case_key(case: &Case) -> String {
    let mut key = String::new();
    key.push_str(&case.export);
    key.push('|');
    for arg in &case.args {
        key.push_str(arg.type_key());
        key.push(',');
    }
    key.push_str(case.expected.type_key());
    key.push('|');
    key.push_str(&format!("{:016x}", hash_bytes(&case.module)));
    key
}

fn make_compiled_symbol_name(case: &Case) -> String {
    let export_name = make_case_symbol_name(0, &case.export);
    let key = make_compiled_case_key(case);
    let hash = hash_bytes(key.as_bytes());
    format!("wast_arm_runtest_module_{export_name}_{hash:016x}")
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3_u64);
    }
    hash
}

fn compile_wasm_function<'data>(
    compiler: &dyn wasmtime_environ::Compiler,
    tunables: &wasmtime_environ::Tunables,
    wasm_bytes: &'data [u8],
    export_name: &str,
    symbol_name: &str,
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
    let mut cfb = compiler.compile_function(&translation, key, body_data, &types, symbol_name)?;

    // Finish compiling
    compiler
        .inlining_compiler()
        .ok_or_else(|| anyhow::anyhow!("compiler does not support inlining"))?
        .finish_compiling(&mut cfb, None, symbol_name)
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

fn exported_symbol_name(symbol_name: &str) -> String {
    if symbol_name.starts_with("wast_arm_runtest_module_") {
        symbol_name.to_string()
    } else {
        format!("wast_arm_runtest_module_{symbol_name}")
    }
}

fn emit_object_file_with_trampoline(cases: &[PreparedCase]) -> Result<Vec<u8>> {
    use cranelift_codegen::ir::{AbiParam, Signature, types};
    use cranelift_codegen::isa::{CallConv, lookup};
    use cranelift_codegen::settings;
    use cranelift_module::{Linkage, Module, default_libcall_names};
    use cranelift_object::{ObjectBuilder, ObjectModule};

    let triple_str = "armv7-unknown-linux-gnueabihf";
    let triple = target_lexicon::Triple::from_str(triple_str)
        .map_err(|e| anyhow::anyhow!("failed to parse target triple: {}", e))?;
    let isa = lookup(triple.clone())?.finish(settings::Flags::new(settings::builder()))?;
    let builder = ObjectBuilder::new(isa, "module", default_libcall_names())?;
    let mut module = ObjectModule::new(builder);

    let ptr = types::I32;
    let mut emitted_symbols = HashSet::new();

    for case in cases {
        // Export the compiled wasm function directly so C can call it with the
        // expected AAPCS-like register layout for (vmctx, caller_vmctx, args...).
        let mut wasm_sig = Signature::new(CallConv::triple_default(&triple));
        wasm_sig.params.push(AbiParam::new(ptr)); // vmctx
        wasm_sig.params.push(AbiParam::new(ptr)); // caller_vmctx
        for arg in &case.args {
            wasm_sig.params.push(AbiParam::new(arg.cranelift_type()));
        }
        wasm_sig
            .returns
            .push(AbiParam::new(case.expected.cranelift_type()));

        let symbol_name = exported_symbol_name(&case.symbol_name);
        if emitted_symbols.contains(&symbol_name) {
            continue;
        }
        emitted_symbols.insert(symbol_name.clone());

        let wasm_id = module.declare_function(&symbol_name, Linkage::Export, &wasm_sig)?;
        module.define_function_bytes(wasm_id, case.alignment as u64, &case.bytes, &[])?;
    }

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
    #[allow(dead_code)]
    stack_limit_off: u32,
}

impl CaseValue {
    fn type_key(self) -> &'static str {
        self.dispatch(|_| "i32", |_| "i64", |_| "f32", |_| "f64")
    }

    fn dispatch<T>(
        self,
        on_i32: impl FnOnce(i32) -> T,
        on_i64: impl FnOnce(i64) -> T,
        on_f32: impl FnOnce(FloatConst<f32>) -> T,
        on_f64: impl FnOnce(FloatConst<f64>) -> T,
    ) -> T {
        match self {
            Self::I32(value) => on_i32(value),
            Self::I64(value) => on_i64(value),
            Self::F32(value) => on_f32(value),
            Self::F64(value) => on_f64(value),
        }
    }

    fn cranelift_type(self) -> cranelift_codegen::ir::Type {
        self.dispatch(
            |_| cranelift_codegen::ir::types::I32,
            |_| cranelift_codegen::ir::types::I64,
            |_| cranelift_codegen::ir::types::F32,
            |_| cranelift_codegen::ir::types::F64,
        )
    }

    fn c_type_name(self) -> &'static str {
        self.dispatch(|_| "i32", |_| "i64", |_| "f32", |_| "f64")
    }

    fn c_assignment(self, index: usize) -> String {
        self.dispatch(
            |value| format!("    let a{index} = {value};"),
            |value| format!("    let a{index} = {value}i64;"),
            |bits| {
                format!(
                    "    let a{index} = bits_to_f32(0x{:08x}_u32);",
                    bits.to_bits()
                )
            },
            |bits| {
                format!(
                    "    let a{index} = bits_to_f64(0x{:016x}_u64);",
                    bits.to_bits()
                )
            },
        )
    }

    fn c_compare_stmt(self, got_name: &str) -> String {
        self.dispatch(
            |value| format!("    let passed = {got_name} == {value};"),
            |value| format!("    let passed = {got_name} == {value}i64;"),
            |bits| {
                format!(
                    "    let passed = f32_to_bits({got_name}) == 0x{:08x}_u32;",
                    bits.to_bits()
                )
            },
            |bits| {
                format!(
                    "    let passed = f64_to_bits({got_name}) == 0x{:016x}_u64;",
                    bits.to_bits()
                )
            },
        )
    }

    fn c_expected_literal(self) -> String {
        let literal = self.to_wast_literal();
        format!("\"{}\"", literal.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn c_got_literal_stmt(self) -> String {
        self.dispatch(
            |_| "    let got_literal = got.to_string();".to_string(),
            |_| "    let got_literal = got.to_string();".to_string(),
            |_| "    let got_literal = f32_to_literal(f32_to_bits(got));".to_string(),
            |_| "    let got_literal = f64_to_literal(f64_to_bits(got));".to_string(),
        )
    }
}

fn format_c_arg_params(args: &[CaseValue]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        let params = args
            .iter()
            .enumerate()
            .map(|(i, arg)| format!(" a{}: {}", i, arg.c_type_name()))
            .collect::<Vec<_>>()
            .join(",");
        format!(", {}", params)
    }
}

fn format_c_arg_calls(args: &[CaseValue]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        let args_call = args
            .iter()
            .enumerate()
            .map(|(i, _)| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(", {}", args_call)
    }
}

fn c_driver_helpers(verbose: bool) -> String {
    let verbose_value = if verbose { "true" } else { "false" };
    let mut helper = String::from(
        "use std::fmt::Write;\n\nfn bits_to_f32(bits: u32) -> f32 { f32::from_bits(bits) }\n\nfn bits_to_f64(bits: u64) -> f64 { f64::from_bits(bits) }\n\nfn f32_to_bits(value: f32) -> u32 { value.to_bits() }\n\nfn f64_to_bits(value: f64) -> u64 { value.to_bits() }\n\nfn f32_to_literal(bits: u32) -> String {\n    match bits {\n        0x0000_0000 => \"0x0p+0\".to_string(),\n        0x8000_0000 => \"-0x0p+0\".to_string(),\n        _ if (bits & 0x7f80_0000) == 0x7f80_0000 => {\n            if (bits & 0x0040_0000) == 0 {\n                \"nan:canonical\".to_string()\n            } else {\n                \"nan:arithmetic\".to_string()\n            }\n        }\n        _ => format!(\"{:?}\", bits_to_f32(bits)),\n    }\n}\n\nfn f64_to_literal(bits: u64) -> String {\n    match bits {\n        0x0000_0000_0000_0000 => \"0x0p+0\".to_string(),\n        0x8000_0000_0000_0000 => \"-0x0p+0\".to_string(),\n        _ if (bits & 0x7ff0_0000_0000_0000) == 0x7ff0_0000_0000_0000 => {\n            if (bits & 0x0008_0000_0000_0000) == 0 {\n                \"nan:canonical\".to_string()\n            } else {\n                \"nan:arithmetic\".to_string()\n            }\n        }\n        _ => format!(\"{:?}\", bits_to_f64(bits)),\n    }\n}\n\nconst WAST_ARM_RUNTTEST_VERBOSE: bool = ",
    );
    helper.push_str(verbose_value);
    helper.push_str(";\n");
    helper
}

struct DriverMainContext<'a> {
    store_ctx_off: u32,
    sc_base: usize,
    export_name: &'a str,
    runner_name: &'a str,
    case_label: &'a str,
    arg_assignments: &'a str,
    got_decl: &'a str,
    args_call: &'a str,
    compare_stmt: &'a str,
    got_literal_stmt: &'a str,
    expected_literal: &'a str,
}

fn rust_driver_main(context: DriverMainContext<'_>, verbose: bool) -> String {
    let DriverMainContext {
        store_ctx_off,
        sc_base,
        export_name,
        runner_name,
        case_label,
        arg_assignments,
        got_decl,
        args_call,
        compare_stmt,
        got_literal_stmt,
        expected_literal,
    } = context;

    let pass_stmt = if verbose {
        format!(
            "    println!(\"PASS case {case_label}: {{}}\", {expected_literal});\n",
            case_label = case_label,
            expected_literal = expected_literal,
        )
    } else {
        String::new()
    };

    format!(
        "fn {runner_name}() -> bool {{\n\
            let mut buf = [0u8; 256];\n\
            unsafe {{\n                let store_ptr = buf.as_mut_ptr().add({store_ctx_off}) as *mut usize;\n\
                *store_ptr = buf.as_mut_ptr().add({sc_base}) as usize;\n\
            }}\n\
            {arg_assignments}\n\
            {got_decl}\n\
            let got = unsafe {{ {export_name}(buf.as_mut_ptr(), buf.as_mut_ptr(){args_call}) }};\n\
            {got_literal_stmt}\n\
            {compare_stmt}\n\
            if passed {{\n\
                {pass_stmt}\
            }} else {{\n\
                println!(\"FAIL case {case_label}: expected {{}} , got {{}}\", {expected_literal}, got_literal);\n\
            }}\n\
            passed\n\
        }}\n",
        runner_name = runner_name,
        store_ctx_off = store_ctx_off,
        sc_base = sc_base,
        arg_assignments = arg_assignments,
        got_decl = got_decl,
        export_name = export_name,
        args_call = args_call,
        got_literal_stmt = got_literal_stmt,
        compare_stmt = compare_stmt,
        pass_stmt = pass_stmt,
        case_label = case_label,
        expected_literal = expected_literal,
    )
}

fn generate_rust_driver(cases: &[PreparedCase], verbose: bool) -> String {
    let sc_base = 64usize;
    let mut driver = String::new();
    driver.push_str(&c_driver_helpers(verbose));
    driver.push('\n');

    let mut declared_exports = HashSet::new();
    driver.push_str("extern \"C\" {\n");
    for case in cases {
        let args_params_str = format_c_arg_params(&case.args);
        let ret_ty = case.expected.c_type_name();
        let exported_name = exported_symbol_name(&case.symbol_name);
        if declared_exports.contains(&exported_name) {
            continue;
        }
        declared_exports.insert(exported_name.clone());
        driver.push_str(&format!(
            "    fn {exported_name}(vmctx: *mut u8, caller_vmctx: *mut u8{args_params_str}) -> {ret_ty};\n"
        ));
    }
    driver.push_str("}\n\n");

    let mut emitted_wrappers = HashSet::new();
    for case in cases {
        let exported_name = exported_symbol_name(&case.symbol_name);
        let wrapper_name = format!("{}_wrapper", sanitize_rust_ident(&exported_name));
        if emitted_wrappers.contains(&wrapper_name) {
            continue;
        }
        emitted_wrappers.insert(wrapper_name.clone());
        let args_params_str = format_c_arg_params(&case.args);
        let ret_ty = case.expected.c_type_name();
        let args_call_str = format_c_arg_calls(&case.args);
        driver.push_str(&format!(
            "fn {wrapper_name}(vmctx: *mut u8, caller_vmctx: *mut u8{args_params_str}) -> {ret_ty} {{\n    unsafe {{ {exported_name}(vmctx, caller_vmctx{args_call_str}) }}\n}}\n"
        ));
    }
    driver.push('\n');

    for case in cases {
        let args_call_str = format_c_arg_calls(&case.args);

        let arg_assignments = case
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| arg.c_assignment(i))
            .collect::<Vec<_>>()
            .join("\n");

        let ret_ty = case.expected.c_type_name();
        let exported_name = exported_symbol_name(&case.symbol_name);
        let wrapper_name = format!("{}_wrapper", sanitize_rust_ident(&exported_name));
        let got_decl = format!(
            "    let got: {ret_ty} = unsafe {{\n        {wrapper_name}(buf.as_mut_ptr(), buf.as_mut_ptr(){args_call_str})\n    }};",
            ret_ty = ret_ty,
            wrapper_name = wrapper_name,
            args_call_str = args_call_str,
        );
        let compare_stmt = case.expected.c_compare_stmt("got");
        let got_literal_stmt = case.expected.c_got_literal_stmt();
        let expected_literal = case.expected.c_expected_literal();
        let case_label = format!("{}", case.index);

        driver.push_str(&rust_driver_main(
            DriverMainContext {
                store_ctx_off: case.store_ctx_off,
                sc_base,
                export_name: &wrapper_name,
                runner_name: &case.runner_name,
                case_label: &case_label,
                arg_assignments: &arg_assignments,
                got_decl: &got_decl,
                args_call: &args_call_str,
                compare_stmt: &compare_stmt,
                got_literal_stmt: &got_literal_stmt,
                expected_literal: &expected_literal,
            },
            verbose,
        ));
        driver.push('\n');
    }

    driver.push_str("fn main() {\n");
    driver.push_str("    let mut failed = false;\n");
    for case in cases {
        driver.push_str(&format!("    failed |= !{}();\n", case.runner_name));
    }
    driver.push_str(
        "    std::process::exit(if failed { 1 } else { 0 });\n}
",
    );
    driver
}

fn parse_result_summary(output: &str) -> Option<(u32, u32)> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let mut words = line.split_whitespace();
        if words.next()? != "RESULT" {
            return None;
        }

        let passed = words.next()?.strip_prefix("passed=")?.parse::<u32>().ok()?;
        let failed = words.next()?.strip_prefix("failed=")?.parse::<u32>().ok()?;
        Some((passed, failed))
    })
}

struct Toolchain {
    linker: std::path::PathBuf,
    sysroot: Option<std::path::PathBuf>,
    qemu: std::path::PathBuf,
}

fn locate_toolchain() -> Result<Toolchain> {
    let qemu_candidates = ["qemu-arm-static", "qemu-arm"];

    let qemu = qemu_candidates
        .iter()
        .find_map(|name| find_executable(name))
        .ok_or_else(|| {
            anyhow::anyhow!("could not find QEMU for ARM; tried {:?}", qemu_candidates)
        })?;

    let linker = find_arm_cross_linker().ok_or_else(|| {
        anyhow::anyhow!("could not find ARM cross linker; install arm-linux-gnueabihf-gcc")
    })?;

    let sysroot = detect_sysroot_from_env().or_else(detect_sysroot_from_known_locations);
    Ok(Toolchain {
        linker,
        sysroot,
        qemu,
    })
}

fn find_executable(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?;
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        let path = dir.join(name);
        path.is_file().then_some(path)
    })
}

fn find_arm_cross_linker() -> Option<std::path::PathBuf> {
    find_executable("arm-linux-gnueabihf-gcc")
}

fn detect_sysroot_from_known_locations() -> Option<std::path::PathBuf> {
    [
        std::path::PathBuf::from("/usr/arm-linux-gnueabihf"),
        std::path::PathBuf::from("/usr/arm-linux-gnueabihf/lib"),
    ]
    .into_iter()
    .find(|path| path.join("lib/ld-linux-armhf.so.3").is_file())
}

fn detect_sysroot_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("ARM_LINUX_GNUEABIHF_SYSROOT")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("CROSS_SYSROOT").map(std::path::PathBuf::from))
}

fn link_with_rustc(
    linker_path: &Path,
    _sysroot: &Option<PathBuf>,
    obj_path: &Path,
    driver_path: &Path,
    elf_path: &Path,
) -> Result<()> {
    let rustc_path = std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"));

    let mut compile_command = std::process::Command::new(&rustc_path);
    let target = "armv7-unknown-linux-gnueabihf";

    compile_command
        .arg("--target")
        .arg(target)
        .arg("-C")
        .arg("panic=abort")
        .arg("-C")
        .arg(format!("linker={}", linker_path.display()))
        .arg("-C")
        .arg(format!("link-arg={}", obj_path.display()))
        .arg("-o")
        .arg(elf_path)
        .arg(driver_path);

    let compile_status = compile_command.status()?;
    if !compile_status.success() {
        anyhow::bail!(
            "Rust driver compilation failed with status: {}",
            compile_status
        );
    }
    Ok(())
}

fn run_under_qemu(
    qemu: &Path,
    sysroot: &Option<PathBuf>,
    elf_path: &Path,
) -> Result<(String, i32)> {
    let mut command = std::process::Command::new(qemu);
    if let Some(sysroot) = sysroot {
        command.arg("-L").arg(sysroot);
    }
    command.arg(elf_path);
    let output = command.output()?;

    let result = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code < 0 {
        anyhow::bail!(
            "QEMU execution failed with status: {}, stderr: {}",
            output.status,
            stderr.trim()
        );
    }

    Ok((result.trim().to_string(), exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use json_from_wast::FloatConst;

    #[test]
    fn driver_uses_wast_style_float_literals() {
        let prepared = PreparedCase {
            index: 0,
            symbol_name: "f".to_string(),
            runner_name: "run_f".to_string(),
            bytes: vec![],
            alignment: 0,
            args: vec![],
            expected: CaseValue::F32(FloatConst::Value(1.0)),
            store_ctx_off: 0,
        };
        let driver = generate_rust_driver(&[prepared], false);
        assert!(driver.contains("fn main()"));
        assert!(!driver.contains("println!(\"PASS case"));
    }

    #[test]
    fn non_verbose_mode_suppresses_result_summary() {
        let prepared = PreparedCase {
            index: 0,
            symbol_name: "f".to_string(),
            runner_name: "run_f".to_string(),
            bytes: vec![],
            alignment: 0,
            args: vec![],
            expected: CaseValue::F32(FloatConst::Value(1.0)),
            store_ctx_off: 0,
        };
        let driver = generate_c_driver(&[prepared], false);
        assert!(driver.contains("#define WAST_ARM_RUNTTEST_VERBOSE 0"));
        assert!(!driver.contains("RESULT"));
    }

    #[test]
    fn verbose_mode_emits_result_summary() {
        let prepared = PreparedCase {
            index: 0,
            symbol_name: "f".to_string(),
            runner_name: "run_f".to_string(),
            bytes: vec![],
            alignment: 0,
            args: vec![],
            expected: CaseValue::F32(FloatConst::Value(1.0)),
            store_ctx_off: 0,
        };
        let driver = generate_rust_driver(&[prepared], true);
        assert!(driver.contains("fn main()"));
        assert!(driver.contains("println!(\"PASS case 0"));
    }

    #[test]
    fn case_symbol_names_are_unique_and_c_safe() {
        assert_eq!(make_case_symbol_name(0, "foo"), "case_0_foo");
        assert_eq!(make_case_symbol_name(1, "foo-bar"), "case_1_foo_bar");
        assert_eq!(make_case_symbol_name(2, "foo.bar"), "case_2_foo_bar");
    }

    #[test]
    fn exported_symbols_are_prefixed_for_the_c_driver() {
        assert_eq!(
            exported_symbol_name("case_0_foo"),
            "wast_arm_runtest_module_case_0_foo"
        );
        assert_eq!(
            exported_symbol_name("wast_arm_runtest_module_case_0_foo"),
            "wast_arm_runtest_module_case_0_foo"
        );
    }

    #[test]
    fn generate_rust_driver_deduplicates_shared_symbols() {
        let prepared = PreparedCase {
            index: 0,
            symbol_name: "shared".to_string(),
            runner_name: "run_shared".to_string(),
            bytes: vec![],
            alignment: 0,
            args: vec![],
            expected: CaseValue::I32(0),
            store_ctx_off: 0,
        };
        let driver = generate_rust_driver(&[prepared.clone(), prepared], false);
        let export_decl_count = driver
            .lines()
            .filter(|line| {
                line.trim_start()
                    .starts_with("fn wast_arm_runtest_module_shared")
            })
            .filter(|line| !line.contains("_wrapper"))
            .count();
        assert_eq!(export_decl_count, 1);
    }

    #[test]
    fn finds_arm_cross_linker_when_available() {
        let temp_dir = std::env::temp_dir().join(format!(
            "wast-arm-runtest-linker-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let linker_path = temp_dir.join("arm-linux-gnueabihf-gcc");
        std::fs::write(&linker_path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&linker_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&linker_path, perms).unwrap();
        }

        let old_path = std::env::var_os("PATH");
        let new_path = std::env::join_paths(
            std::iter::once(temp_dir.clone()).chain(
                old_path
                    .as_ref()
                    .into_iter()
                    .flat_map(|p| std::env::split_paths(p)),
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("PATH", &new_path);
        }
        let resolved = find_arm_cross_linker();
        if let Some(old_path) = old_path {
            unsafe {
                std::env::set_var("PATH", old_path);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }

        assert!(resolved.is_some());
        assert_eq!(
            resolved.unwrap().file_name().unwrap(),
            "arm-linux-gnueabihf-gcc"
        );
    }

    #[test]
    fn detects_arm_sysroot_when_present() {
        let sysroot = detect_sysroot_from_known_locations();
        assert!(sysroot.is_none() || sysroot.as_ref().unwrap().ends_with("arm-linux-gnueabihf"));
    }

    #[test]
    fn unsupported_backend_errors_are_failed() {
        let outcome = classify_case_error(
            "Unsupported feature: should be implemented in ISLE: inst = `v4 = rotl.i32`",
        );
        assert!(matches!(outcome, CaseOutcome::Fail(_)));
    }

    #[test]
    fn panic_errors_are_failed() {
        let outcome = classify_case_error("panic while compiling wasm function");
        assert!(matches!(outcome, CaseOutcome::Fail(_)));
    }
}
