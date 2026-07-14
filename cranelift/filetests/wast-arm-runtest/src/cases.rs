use json_from_wast::{Const, CoreConst};
use wasmparser::Parser;

#[derive(Debug)]
pub struct Case {
    pub module: Vec<u8>,
    pub export: String,
    pub args: Vec<i32>,
    pub expected: i32,
}

impl Case {
    pub fn try_build(
        module: &[u8],
        field: &str,
        args: &[Const],
        expected: &[Const],
        verbose: bool,
    ) -> Result<Option<Case>, String> {
        let args: Vec<i32> = args
            .iter()
            .map(|a| match a {
                Const::Core(CoreConst::I32 { value }) => Ok(value.0),
                other => Err(format!("non-i32 arg: {other:?}")),
            })
            .collect::<Result<Vec<i32>, String>>()?;

        if expected.len() != 1 {
            return Err(format!("expected 1 result, got {}", expected.len()));
        }
        let expected = match &expected[0] {
            Const::Core(CoreConst::I32 { value }) => value.0,
            other => return Err(format!("non-i32 result: {other:?}")),
        };

        if !module_exports_func(module, field) {
            return Err(format!("no func export named {field:?}"));
        }

        Ok(Some(Case {
            module: module.to_vec(),
            export: field.to_string(),
            args,
            expected,
        }))
    }
}

fn module_exports_func(module: &[u8], name: &str) -> bool {
    let mut parser = Parser::new(0);
    for payload in parser.parse_all(module) {
        if let Ok(payload) = payload {
            if let wasmparser::Payload::ExportSection(reader) = payload {
                for export in reader {
                    if let Ok(export) = export {
                        if export.kind == wasmparser::ExternalKind::Func && export.name == name {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}
