//! ARM32 ISA: binary code emission.

use cranelift_control::ControlPlane;

use crate::Reg;
use crate::isa::arm32::inst::AMode;
use crate::isa::arm32::inst::regs::x_reg;
use crate::isa::arm32::{self, abi::Arm32MachineDeps, inst::Inst};
use crate::{
    FrameLayout, MachBuffer, MachInstEmit, MachInstEmitState, ir,
    machinst::{Callee, MachInst},
    settings,
};

pub struct EmitInfo {
    #[expect(dead_code, reason = "will be used in the future")]
    shared_flags: settings::Flags,
    _isa_flags: arm32::settings::Flags,
}

impl EmitInfo {
    pub(crate) fn new(shared_flags: settings::Flags, isa_flags: arm32::settings::Flags) -> Self {
        EmitInfo {
            shared_flags,
            _isa_flags: isa_flags,
        }
    }
}

/// Stub state carried between emissions of a sequence of instructions.
#[derive(Default, Clone, Debug)]
pub struct EmitState {
    /// The user stack map for the upcoming instruction.
    _user_stack_map: Option<ir::UserStackMap>,
    /// Control plane (stub - not fully functional).
    ctrl_plane: ControlPlane,
    frame_layout: FrameLayout,
}

impl MachInstEmitState<Inst> for EmitState {
    fn new(_abi: &Callee<Arm32MachineDeps>, ctrl_plane: ControlPlane) -> Self {
        EmitState {
            _user_stack_map: None,
            ctrl_plane,
            frame_layout: FrameLayout::default(),
        }
    }

    fn pre_safepoint(&mut self, user_stack_map: Option<ir::UserStackMap>) {
        self._user_stack_map = user_stack_map;
    }

    fn ctrl_plane_mut(&mut self) -> &mut ControlPlane {
        &mut self.ctrl_plane
    }

    fn take_ctrl_plane(self) -> ControlPlane {
        self.ctrl_plane
    }

    fn frame_layout(&self) -> &FrameLayout {
        &self.frame_layout
    }
}

/// Emit a single TrapCode (unconditional trap).
fn emit_trap(sink: &mut MachBuffer<Inst>) {
    sink.put2(0xBE00);
}

/// Operation type for emit_ldst_32 (32-bit load/store)
#[derive(Clone, Copy)]
enum LdSt32Op {
    /// 32-bit load (LDR)
    Ldr,
    /// 32-bit store (STR)
    Str,
}

impl LdSt32Op {
    /// Returns true for store operations
    fn is_store(self) -> bool {
        matches!(self, LdSt32Op::Str)
    }

    /// Returns the narrow encoding opcode base
    fn narrow_opcode_base(self) -> u32 {
        if self.is_store() { 0x6000 } else { 0x6800 }
    }

    /// Returns the wide encoding opcode base
    fn wide_opcode_base(self) -> u32 {
        if self.is_store() { 0xF8C0 } else { 0xF8D0 }
    }
}

/// Emit wide T3/T4 encoding for 32-bit loads/stores
fn emit_wide_ldst_32(sink: &mut MachBuffer<Inst>, op: LdSt32Op, rn: u32, rd: u32, offset: i64) {
    // For negative offsets, use different first opcode (clear bit 7)
    let (first_opcode, second_base) = if offset >= 0 {
        (
            op.wide_opcode_base() | rn,
            rd << 12 | (offset as u32 & 0xFF),
        )
    } else {
        let neg_imm = ((-offset / 4) & 0xFFF) as u32;
        // Clear bit 7 by XORing with 0x0080
        (
            op.wide_opcode_base() ^ 0x0080 | rn,
            0x0C00 | (rd << 12) | neg_imm,
        )
    };

    emit_ldst_halfwords(sink, first_opcode as u16, second_base as u16);
}

/// Emit narrow or wide encoding for 32-bit loads/stores based on offset range
fn emit_ldst_32(sink: &mut MachBuffer<Inst>, op: LdSt32Op, rn: u32, rd: u32, offset: i64) {
    // Check if narrow encoding is possible (0-4096, multiple of 4)
    if (0..=4096).contains(&offset) && offset % 4 == 0 && offset >= 0 {
        // Use narrow T3 encoding
        let imm12 = (offset / 4) as u32 & 0xFFF;
        let enc = op.narrow_opcode_base() | (rd << 8) | imm12;
        sink.put2(enc as u16);
        sink.put2((rn << 12 | imm12) as u16);
    } else if offset >= -256 && offset < 0 {
        // Use wide T3 encoding with negative offset
        emit_wide_ldst_32(sink, op, rn, rd, offset);
    } else {
        panic!("LDR/STR offset {offset} out of range");
    }
}

/// Extract the register number from a Reg (for Thumb encoding).
fn reg_num(reg: Reg) -> u32 {
    match reg.to_real_reg() {
        Some(r) => r.hw_enc() as u32,
        None => 0, // fallback; should not happen for allocated regs
    }
}

/// Emit two halfwords for wide T3/T4 load/store encoding
fn emit_ldst_halfwords(sink: &mut MachBuffer<Inst>, first: u16, second: u16) {
    sink.put2(first);
    sink.put2(second);
}

fn ldr_from_offset(sink: &mut MachBuffer<Inst>, rd: Reg, base: Reg, offset: i64) {
    let rdn = reg_num(rd);
    let bsn = reg_num(base);
    emit_ldst_32(sink, LdSt32Op::Ldr, bsn, rdn, offset);
}

fn str_from_offset(sink: &mut MachBuffer<Inst>, rs: Reg, base: Reg, offset: i64) {
    let rsn = reg_num(rs);
    let bsn = reg_num(base);
    emit_ldst_32(sink, LdSt32Op::Str, bsn, rsn, offset);
}

impl MachInstEmit for Inst {
    type State = EmitState;
    type Info = EmitInfo;

    fn emit(&self, sink: &mut MachBuffer<Self>, _info: &Self::Info, _state: &mut Self::State) {
        match self {
            Inst::Ret => {
                // RET = BX LR: 0b0100_0111_0111_0000 = 0x4770
                sink.put2(0x4770);
            }

            // Rets is a pseudo-instruction that constrains return registers; it emits no bytes.
            Inst::Rets { .. } => {}

            // Args is a pseudo-instruction that defines arg registers; emits no bytes.
            Inst::Args { .. } => {}

            // Push registers — wide STMDB.W sp!, {list}: 0xE92D | reg_list.
            Inst::Push { rs } => {
                sink.put2(0xE92D);
                sink.put2(*rs);
            }

            // Pop registers — wide LDMIA.W sp!, {list}: 0xE8BD | reg_list.
            Inst::Pop { rt } => {
                sink.put2(0xE8BD);
                sink.put2(*rt);
            }

            Inst::Load { rd, mem, ty: _ty } => match mem {
                AMode::RegOffset(base, offset) => {
                    ldr_from_offset(sink, rd.to_reg(), *base, *offset)
                }
                AMode::SPOffset(offset) => ldr_from_offset(sink, rd.to_reg(), x_reg(13), *offset),
            },

            Inst::Store { rs, mem, ty: _ty } => match mem {
                AMode::RegOffset(base, offset) => str_from_offset(sink, *rs, *base, *offset),
                AMode::SPOffset(offset) => str_from_offset(sink, *rs, x_reg(13), *offset),
            },


        }
        if self.is_trap() {
            emit_trap(sink);
        }
    }

    fn pretty_print_inst(&self, _state: &mut Self::State) -> std::prelude::v1::String {
        format!("{self:?}")
    }
}
