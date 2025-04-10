use cfg_if::cfg_if;
use cranelift_codegen::cursor::FuncCursor;
use cranelift_codegen::ir;
use cranelift_codegen::ir::condcodes::*;
use cranelift_codegen::ir::immediates::{Imm64, Offset32, Uimm64};
use cranelift_codegen::ir::pcc::Fact;
use cranelift_codegen::ir::types::*;
use cranelift_codegen::ir::{
    AbiParam, ArgumentPurpose, Function, InstBuilder, MemFlags, Signature, UserFuncName, Value,
};
use cranelift_codegen::isa::{self, CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_entity::{EntityRef, PrimaryMap};
use cranelift_frontend::FunctionBuilder;
use cranelift_frontend::Variable;
use cranelift_wasm::{
    self, FuncIndex, FuncTranslationState, GlobalIndex, GlobalVariable, Heap, HeapData, HeapStyle,
    MemoryIndex, TableIndex, TargetEnvironment, TypeIndex, WasmHeapType, WasmRefType, WasmResult,
    WasmValType,
};
use std::convert::TryFrom;
use std::mem;
use wasmparser::Operator;
use wasmtime_environ::{
    BuiltinFunctionIndex, MemoryPlan, MemoryStyle, Module, ModuleTranslation, ModuleTypesBuilder,
    PtrSize, TableStyle, Tunables, TypeConvert, VMOffsets, WASM_PAGE_SIZE,
};
use wasmtime_environ::{FUNCREF_INIT_BIT, FUNCREF_MASK};

macro_rules! declare_function_signatures {
    (
        $(
            $( #[$attr:meta] )*
            $name:ident( $( $pname:ident: $param:ident ),* ) $( -> $result:ident )?;
        )*
    ) => {
        /// A struct with an `Option<ir::SigRef>` member for every builtin
        /// function, to de-duplicate constructing/getting its signature.
        struct BuiltinFunctionSignatures {
            pointer_type: ir::Type,
            reference_type: ir::Type,
            call_conv: isa::CallConv,
            $(
                $name: Option<ir::SigRef>,
            )*
        }

        impl BuiltinFunctionSignatures {
            fn new(
                pointer_type: ir::Type,
                reference_type: ir::Type,
                call_conv: isa::CallConv,
            ) -> Self {
                Self {
                    pointer_type,
                    reference_type,
                    call_conv,
                    $(
                        $name: None,
                    )*
                }
            }

            fn vmctx(&self) -> AbiParam {
                AbiParam::special(self.pointer_type, ArgumentPurpose::VMContext)
            }

            fn reference(&self) -> AbiParam {
                AbiParam::new(self.reference_type)
            }

            fn pointer(&self) -> AbiParam {
                AbiParam::new(self.pointer_type)
            }

            fn i32(&self) -> AbiParam {
                // Some platform ABIs require i32 values to be zero- or sign-
                // extended to the full register width.  We need to indicate
                // this here by using the appropriate .uext or .sext attribute.
                // The attribute can be added unconditionally; platforms whose
                // ABI does not require such extensions will simply ignore it.
                // Note that currently all i32 arguments or return values used
                // by builtin functions are unsigned, so we always use .uext.
                // If that ever changes, we will have to add a second type
                // marker here.
                AbiParam::new(I32).uext()
            }

            fn i64(&self) -> AbiParam {
                AbiParam::new(I64)
            }

            $(
                fn $name(&mut self, func: &mut Function) -> ir::SigRef {
                    let sig = self.$name.unwrap_or_else(|| {
                        func.import_signature(Signature {
                            params: vec![ $( self.$param() ),* ],
                            returns: vec![ $( self.$result() )? ],
                            call_conv: self.call_conv,
                        })
                    });
                    self.$name = Some(sig);
                    sig
                }
            )*
        }
    };
}

wasmtime_environ::foreach_builtin_function!(declare_function_signatures);

/// The `FuncEnvironment` implementation for use by the `ModuleEnvironment`.
pub struct FuncEnvironment<'module_environment> {
    isa: &'module_environment (dyn TargetIsa + 'module_environment),
    module: &'module_environment Module,
    types: &'module_environment ModuleTypesBuilder,

    translation: &'module_environment ModuleTranslation<'module_environment>,

    /// Heaps implementing WebAssembly linear memories.
    heaps: PrimaryMap<Heap, HeapData>,

    /// The Cranelift global holding the vmctx address.
    vmctx: Option<ir::GlobalValue>,

    /// The PCC memory type describing the vmctx layout, if we're
    /// using PCC.
    pcc_vmctx_memtype: Option<ir::MemoryType>,

    /// Caches of signatures for builtin functions.
    builtin_function_signatures: BuiltinFunctionSignatures,

    /// Offsets to struct fields accessed by JIT code.
    pub(crate) offsets: VMOffsets<u8>,

    tunables: &'module_environment Tunables,

    /// A function-local variable which stores the cached value of the amount of
    /// fuel remaining to execute. If used this is modified frequently so it's
    /// stored locally as a variable instead of always referenced from the field
    /// in `*const VMRuntimeLimits`
    fuel_var: cranelift_frontend::Variable,

    /// A function-local variable which caches the value of `*const
    /// VMRuntimeLimits` for this function's vmctx argument. This pointer is stored
    /// in the vmctx itself, but never changes for the lifetime of the function,
    /// so if we load it up front we can continue to use it throughout.
    vmruntime_limits_ptr: cranelift_frontend::Variable,

    /// A cached epoch deadline value, when performing epoch-based
    /// interruption. Loaded from `VMRuntimeLimits` and reloaded after
    /// any yield.
    epoch_deadline_var: cranelift_frontend::Variable,

    /// A cached pointer to the per-Engine epoch counter, when
    /// performing epoch-based interruption. Initialized in the
    /// function prologue. We prefer to use a variable here rather
    /// than reload on each check because it's better to let the
    /// regalloc keep it in a register if able; if not, it can always
    /// spill, and this isn't any worse than reloading each time.
    epoch_ptr_var: cranelift_frontend::Variable,

    fuel_consumed: i64,

    #[cfg(feature = "wmemcheck")]
    wmemcheck: bool,
}

impl<'module_environment> FuncEnvironment<'module_environment> {
    pub fn new(
        isa: &'module_environment (dyn TargetIsa + 'module_environment),
        translation: &'module_environment ModuleTranslation<'module_environment>,
        types: &'module_environment ModuleTypesBuilder,
        tunables: &'module_environment Tunables,
        wmemcheck: bool,
    ) -> Self {
        let builtin_function_signatures = BuiltinFunctionSignatures::new(
            isa.pointer_type(),
            match isa.pointer_type() {
                ir::types::I32 => ir::types::R32,
                ir::types::I64 => ir::types::R64,
                _ => panic!(),
            },
            CallConv::triple_default(isa.triple()),
        );

        // Avoid unused warning in default build.
        #[cfg(not(feature = "wmemcheck"))]
        let _ = wmemcheck;

        Self {
            isa,
            module: &translation.module,
            types,
            heaps: PrimaryMap::default(),
            vmctx: None,
            pcc_vmctx_memtype: None,
            builtin_function_signatures,
            offsets: VMOffsets::new(isa.pointer_bytes(), &translation.module),
            tunables,
            fuel_var: Variable::new(0),
            epoch_deadline_var: Variable::new(0),
            epoch_ptr_var: Variable::new(0),
            vmruntime_limits_ptr: Variable::new(0),
            translation: translation,

            // Start with at least one fuel being consumed because even empty
            // functions should consume at least some fuel.
            fuel_consumed: 1,
            #[cfg(feature = "wmemcheck")]
            wmemcheck,
        }
    }

    fn pointer_type(&self) -> ir::Type {
        self.isa.pointer_type()
    }

    fn vmctx(&mut self, func: &mut Function) -> ir::GlobalValue {
        self.vmctx.unwrap_or_else(|| {
            let vmctx = func.create_global_value(ir::GlobalValueData::VMContext);
            if self.isa.flags().enable_pcc() {
                // Create a placeholder memtype for the vmctx; we'll
                // add fields to it as we lazily create HeapData
                // structs and global values.
                let vmctx_memtype = func.create_memory_type(ir::MemoryTypeData::Struct {
                    size: 0,
                    fields: vec![],
                });

                self.pcc_vmctx_memtype = Some(vmctx_memtype);
                func.global_value_facts[vmctx] = Some(Fact::Mem {
                    ty: vmctx_memtype,
                    min_offset: 0,
                    max_offset: 0,
                    nullable: false,
                });
            }

            self.vmctx = Some(vmctx);
            vmctx
        })
    }

    fn get_table_copy_func(
        &mut self,
        func: &mut Function,
        dst_table_index: TableIndex,
        src_table_index: TableIndex,
    ) -> (ir::SigRef, usize, usize, BuiltinFunctionIndex) {
        let sig = self.builtin_function_signatures.table_copy(func);
        (
            sig,
            dst_table_index.as_u32() as usize,
            src_table_index.as_u32() as usize,
            BuiltinFunctionIndex::table_copy(),
        )
    }

    fn get_table_init_func(
        &mut self,
        func: &mut Function,
        table_index: TableIndex,
    ) -> (ir::SigRef, usize, BuiltinFunctionIndex) {
        let sig = self.builtin_function_signatures.table_init(func);
        let table_index = table_index.as_u32() as usize;
        (sig, table_index, BuiltinFunctionIndex::table_init())
    }

    fn get_elem_drop_func(&mut self, func: &mut Function) -> (ir::SigRef, BuiltinFunctionIndex) {
        let sig = self.builtin_function_signatures.elem_drop(func);
        (sig, BuiltinFunctionIndex::elem_drop())
    }

    fn get_memory_atomic_wait(
        &mut self,
        func: &mut Function,
        memory_index: MemoryIndex,
        ty: ir::Type,
    ) -> (ir::SigRef, usize, BuiltinFunctionIndex) {
        match ty {
            I32 => (
                self.builtin_function_signatures.memory_atomic_wait32(func),
                memory_index.index(),
                BuiltinFunctionIndex::memory_atomic_wait32(),
            ),
            I64 => (
                self.builtin_function_signatures.memory_atomic_wait64(func),
                memory_index.index(),
                BuiltinFunctionIndex::memory_atomic_wait64(),
            ),
            x => panic!("get_memory_atomic_wait unsupported type: {:?}", x),
        }
    }

    fn get_memory_init_func(&mut self, func: &mut Function) -> (ir::SigRef, BuiltinFunctionIndex) {
        (
            self.builtin_function_signatures.memory_init(func),
            BuiltinFunctionIndex::memory_init(),
        )
    }

    fn get_data_drop_func(&mut self, func: &mut Function) -> (ir::SigRef, BuiltinFunctionIndex) {
        (
            self.builtin_function_signatures.data_drop(func),
            BuiltinFunctionIndex::data_drop(),
        )
    }

    /// Translates load of builtin function and returns a pair of values `vmctx`
    /// and address of the loaded function.
    fn translate_load_builtin_function_address(
        &mut self,
        pos: &mut FuncCursor<'_>,
        callee_func_idx: BuiltinFunctionIndex,
    ) -> (ir::Value, ir::Value) {
        // We use an indirect call so that we don't have to patch the code at runtime.
        let pointer_type = self.pointer_type();
        let vmctx = self.vmctx(&mut pos.func);
        let base = pos.ins().global_value(pointer_type, vmctx);

        let mem_flags = ir::MemFlags::trusted().with_readonly();

        // Load the base of the array of builtin functions
        let array_offset = i32::try_from(self.offsets.vmctx_builtin_functions()).unwrap();
        let array_addr = pos.ins().load(pointer_type, mem_flags, base, array_offset);

        // Load the callee address.
        let body_offset = i32::try_from(callee_func_idx.index() * pointer_type.bytes()).unwrap();
        let func_addr = pos
            .ins()
            .load(pointer_type, mem_flags, array_addr, body_offset);

        (base, func_addr)
    }

    /// Generate code to increment or decrement the given `externref`'s
    /// reference count.
    ///
    /// The new reference count is returned.
    fn mutate_externref_ref_count(
        &mut self,
        builder: &mut FunctionBuilder,
        externref: ir::Value,
        delta: i64,
    ) -> ir::Value {
        debug_assert!(delta == -1 || delta == 1);

        let pointer_type = self.pointer_type();

        // If this changes that's ok, the `atomic_rmw` below just needs to be
        // preceded with an add instruction of `externref` and the offset.
        assert_eq!(self.offsets.vm_extern_data_ref_count(), 0);
        let delta = builder.ins().iconst(pointer_type, delta);
        builder.ins().atomic_rmw(
            pointer_type,
            ir::MemFlags::trusted(),
            ir::AtomicRmwOp::Add,
            externref,
            delta,
        )
    }

    fn get_global_location(
        &mut self,
        func: &mut ir::Function,
        index: GlobalIndex,
    ) -> (ir::GlobalValue, i32) {
        let pointer_type = self.pointer_type();
        let vmctx = self.vmctx(func);
        if let Some(def_index) = self.module.defined_global_index(index) {
            let offset = i32::try_from(self.offsets.vmctx_vmglobal_definition(def_index)).unwrap();
            (vmctx, offset)
        } else {
            let from_offset = self.offsets.vmctx_vmglobal_import_from(index);
            let global = func.create_global_value(ir::GlobalValueData::Load {
                base: vmctx,
                offset: Offset32::new(i32::try_from(from_offset).unwrap()),
                global_type: pointer_type,
                flags: MemFlags::trusted().with_readonly(),
            });
            (global, 0)
        }
    }

    fn declare_vmruntime_limits_ptr(&mut self, builder: &mut FunctionBuilder<'_>) {
        // We load the `*const VMRuntimeLimits` value stored within vmctx at the
        // head of the function and reuse the same value across the entire
        // function. This is possible since we know that the pointer never
        // changes for the lifetime of the function.
        let pointer_type = self.pointer_type();
        builder.declare_var(self.vmruntime_limits_ptr, pointer_type);
        let vmctx = self.vmctx(builder.func);
        let base = builder.ins().global_value(pointer_type, vmctx);
        let offset = i32::try_from(self.offsets.vmctx_runtime_limits()).unwrap();
        let interrupt_ptr = builder
            .ins()
            .load(pointer_type, ir::MemFlags::trusted(), base, offset);
        builder.def_var(self.vmruntime_limits_ptr, interrupt_ptr);
    }

    fn fuel_function_entry(&mut self, builder: &mut FunctionBuilder<'_>) {
        // On function entry we load the amount of fuel into a function-local
        // `self.fuel_var` to make fuel modifications fast locally. This cache
        // is then periodically flushed to the Store-defined location in
        // `VMRuntimeLimits` later.
        builder.declare_var(self.fuel_var, ir::types::I64);
        self.fuel_load_into_var(builder);
        self.fuel_check(builder);
    }

    fn fuel_function_exit(&mut self, builder: &mut FunctionBuilder<'_>) {
        // On exiting the function we need to be sure to save the fuel we have
        // cached locally in `self.fuel_var` back into the Store-defined
        // location.
        self.fuel_save_from_var(builder);
    }

    fn fuel_before_op(
        &mut self,
        op: &Operator<'_>,
        builder: &mut FunctionBuilder<'_>,
        reachable: bool,
    ) {
        if !reachable {
            // In unreachable code we shouldn't have any leftover fuel we
            // haven't accounted for since the reason for us to become
            // unreachable should have already added it to `self.fuel_var`.
            debug_assert_eq!(self.fuel_consumed, 0);
            return;
        }

        self.fuel_consumed += match op {
            // Nop and drop generate no code, so don't consume fuel for them.
            Operator::Nop | Operator::Drop => 0,

            // Control flow may create branches, but is generally cheap and
            // free, so don't consume fuel. Note the lack of `if` since some
            // cost is incurred with the conditional check.
            Operator::Block { .. }
            | Operator::Loop { .. }
            | Operator::Unreachable
            | Operator::Return
            | Operator::Else
            | Operator::End => 0,

            // everything else, just call it one operation.
            _ => 1,
        };

        match op {
            // Exiting a function (via a return or unreachable) or otherwise
            // entering a different function (via a call) means that we need to
            // update the fuel consumption in `VMRuntimeLimits` because we're
            // about to move control out of this function itself and the fuel
            // may need to be read.
            //
            // Before this we need to update the fuel counter from our own cost
            // leading up to this function call, and then we can store
            // `self.fuel_var` into `VMRuntimeLimits`.
            Operator::Unreachable
            | Operator::Return
            | Operator::CallIndirect { .. }
            | Operator::Call { .. }
            | Operator::ReturnCall { .. }
            | Operator::ReturnCallIndirect { .. } => {
                self.fuel_increment_var(builder);
                self.fuel_save_from_var(builder);
            }

            // To ensure all code preceding a loop is only counted once we
            // update the fuel variable on entry.
            Operator::Loop { .. }

            // Entering into an `if` block means that the edge we take isn't
            // known until runtime, so we need to update our fuel consumption
            // before we take the branch.
            | Operator::If { .. }

            // Control-flow instructions mean that we're moving to the end/exit
            // of a block somewhere else. That means we need to update the fuel
            // counter since we're effectively terminating our basic block.
            | Operator::Br { .. }
            | Operator::BrIf { .. }
            | Operator::BrTable { .. }

            // Exiting a scope means that we need to update the fuel
            // consumption because there are multiple ways to exit a scope and
            // this is the only time we have to account for instructions
            // executed so far.
            | Operator::End

            // This is similar to `end`, except that it's only the terminator
            // for an `if` block. The same reasoning applies though in that we
            // are terminating a basic block and need to update the fuel
            // variable.
            | Operator::Else => self.fuel_increment_var(builder),

            // This is a normal instruction where the fuel is buffered to later
            // get added to `self.fuel_var`.
            //
            // Note that we generally ignore instructions which may trap and
            // therefore result in exiting a block early. Current usage of fuel
            // means that it's not too important to account for a precise amount
            // of fuel consumed but rather "close to the actual amount" is good
            // enough. For 100% precise counting, however, we'd probably need to
            // not only increment but also save the fuel amount more often
            // around trapping instructions. (see the `unreachable` instruction
            // case above)
            //
            // Note that `Block` is specifically omitted from incrementing the
            // fuel variable. Control flow entering a `block` is unconditional
            // which means it's effectively executing straight-line code. We'll
            // update the counter when exiting a block, but we shouldn't need to
            // do so upon entering a block.
            _ => {}
        }
    }

    fn fuel_after_op(&mut self, op: &Operator<'_>, builder: &mut FunctionBuilder<'_>) {
        // After a function call we need to reload our fuel value since the
        // function may have changed it.
        match op {
            Operator::Call { .. } | Operator::CallIndirect { .. } => {
                self.fuel_load_into_var(builder);
            }
            _ => {}
        }
    }

    /// Adds `self.fuel_consumed` to the `fuel_var`, zero-ing out the amount of
    /// fuel consumed at that point.
    fn fuel_increment_var(&mut self, builder: &mut FunctionBuilder<'_>) {
        let consumption = mem::replace(&mut self.fuel_consumed, 0);
        if consumption == 0 {
            return;
        }

        let fuel = builder.use_var(self.fuel_var);
        let fuel = builder.ins().iadd_imm(fuel, consumption);
        builder.def_var(self.fuel_var, fuel);
    }

    /// Loads the fuel consumption value from `VMRuntimeLimits` into `self.fuel_var`
    fn fuel_load_into_var(&mut self, builder: &mut FunctionBuilder<'_>) {
        let (addr, offset) = self.fuel_addr_offset(builder);
        let fuel = builder
            .ins()
            .load(ir::types::I64, ir::MemFlags::trusted(), addr, offset);
        builder.def_var(self.fuel_var, fuel);
    }

    /// Stores the fuel consumption value from `self.fuel_var` into
    /// `VMRuntimeLimits`.
    fn fuel_save_from_var(&mut self, builder: &mut FunctionBuilder<'_>) {
        let (addr, offset) = self.fuel_addr_offset(builder);
        let fuel_consumed = builder.use_var(self.fuel_var);
        builder
            .ins()
            .store(ir::MemFlags::trusted(), fuel_consumed, addr, offset);
    }

    /// Returns the `(address, offset)` of the fuel consumption within
    /// `VMRuntimeLimits`, used to perform loads/stores later.
    fn fuel_addr_offset(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> (ir::Value, ir::immediates::Offset32) {
        (
            builder.use_var(self.vmruntime_limits_ptr),
            i32::from(self.offsets.ptr.vmruntime_limits_fuel_consumed()).into(),
        )
    }

    /// Checks the amount of remaining, and if we've run out of fuel we call
    /// the out-of-fuel function.
    fn fuel_check(&mut self, builder: &mut FunctionBuilder) {
        self.fuel_increment_var(builder);
        let out_of_gas_block = builder.create_block();
        let continuation_block = builder.create_block();

        // Note that our fuel is encoded as adding positive values to a
        // negative number. Whenever the negative number goes positive that
        // means we ran out of fuel.
        //
        // Compare to see if our fuel is positive, and if so we ran out of gas.
        // Otherwise we can continue on like usual.
        let zero = builder.ins().iconst(ir::types::I64, 0);
        let fuel = builder.use_var(self.fuel_var);
        let cmp = builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, fuel, zero);
        builder
            .ins()
            .brif(cmp, out_of_gas_block, &[], continuation_block, &[]);
        builder.seal_block(out_of_gas_block);

        // If we ran out of gas then we call our out-of-gas intrinsic and it
        // figures out what to do. Note that this may raise a trap, or do
        // something like yield to an async runtime. In either case we don't
        // assume what happens and handle the case the intrinsic returns.
        //
        // Note that we save/reload fuel around this since the out-of-gas
        // intrinsic may alter how much fuel is in the system.
        builder.switch_to_block(out_of_gas_block);
        self.fuel_save_from_var(builder);
        let out_of_gas_sig = self.builtin_function_signatures.out_of_gas(builder.func);
        let (vmctx, out_of_gas) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::out_of_gas(),
        );
        builder
            .ins()
            .call_indirect(out_of_gas_sig, out_of_gas, &[vmctx]);
        self.fuel_load_into_var(builder);
        builder.ins().jump(continuation_block, &[]);
        builder.seal_block(continuation_block);

        builder.switch_to_block(continuation_block);
    }

    fn epoch_function_entry(&mut self, builder: &mut FunctionBuilder<'_>) {
        builder.declare_var(self.epoch_deadline_var, ir::types::I64);
        self.epoch_load_deadline_into_var(builder);
        builder.declare_var(self.epoch_ptr_var, self.pointer_type());
        let epoch_ptr = self.epoch_ptr(builder);
        builder.def_var(self.epoch_ptr_var, epoch_ptr);

        // We must check for an epoch change when entering a
        // function. Why? Why aren't checks at loops sufficient to
        // bound runtime to O(|static program size|)?
        //
        // The reason is that one can construct a "zip-bomb-like"
        // program with exponential-in-program-size runtime, with no
        // backedges (loops), by building a tree of function calls: f0
        // calls f1 ten times, f1 calls f2 ten times, etc. E.g., nine
        // levels of this yields a billion function calls with no
        // backedges. So we can't do checks only at backedges.
        //
        // In this "call-tree" scenario, and in fact in any program
        // that uses calls as a sort of control flow to try to evade
        // backedge checks, a check at every function entry is
        // sufficient. Then, combined with checks at every backedge
        // (loop) the longest runtime between checks is bounded by the
        // straightline length of any function body.
        self.epoch_check(builder);
    }

    #[cfg(feature = "wmemcheck")]
    fn hook_malloc_exit(&mut self, builder: &mut FunctionBuilder, retvals: &[Value]) {
        let check_malloc_sig = self.builtin_function_signatures.check_malloc(builder.func);
        let (vmctx, check_malloc) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::check_malloc(),
        );
        let func_args = builder
            .func
            .dfg
            .block_params(builder.func.layout.entry_block().unwrap());
        let len = if func_args.len() < 3 {
            return;
        } else {
            // If a function named `malloc` has at least one argument, we assume the
            // first argument is the requested allocation size.
            func_args[2]
        };
        let retval = if retvals.len() < 1 {
            return;
        } else {
            retvals[0]
        };
        builder
            .ins()
            .call_indirect(check_malloc_sig, check_malloc, &[vmctx, retval, len]);
    }

    #[cfg(feature = "wmemcheck")]
    fn hook_free_exit(&mut self, builder: &mut FunctionBuilder) {
        let check_free_sig = self.builtin_function_signatures.check_free(builder.func);
        let (vmctx, check_free) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::check_free(),
        );
        let func_args = builder
            .func
            .dfg
            .block_params(builder.func.layout.entry_block().unwrap());
        let ptr = if func_args.len() < 3 {
            return;
        } else {
            // If a function named `free` has at least one argument, we assume the
            // first argument is a pointer to memory.
            func_args[2]
        };
        builder
            .ins()
            .call_indirect(check_free_sig, check_free, &[vmctx, ptr]);
    }

    fn epoch_ptr(&mut self, builder: &mut FunctionBuilder<'_>) -> ir::Value {
        let vmctx = self.vmctx(builder.func);
        let pointer_type = self.pointer_type();
        let base = builder.ins().global_value(pointer_type, vmctx);
        let offset = i32::try_from(self.offsets.vmctx_epoch_ptr()).unwrap();
        let epoch_ptr = builder
            .ins()
            .load(pointer_type, ir::MemFlags::trusted(), base, offset);
        epoch_ptr
    }

    fn epoch_load_current(&mut self, builder: &mut FunctionBuilder<'_>) -> ir::Value {
        let addr = builder.use_var(self.epoch_ptr_var);
        builder.ins().load(
            ir::types::I64,
            ir::MemFlags::trusted(),
            addr,
            ir::immediates::Offset32::new(0),
        )
    }

    fn epoch_load_deadline_into_var(&mut self, builder: &mut FunctionBuilder<'_>) {
        let interrupts = builder.use_var(self.vmruntime_limits_ptr);
        let deadline =
            builder.ins().load(
                ir::types::I64,
                ir::MemFlags::trusted(),
                interrupts,
                ir::immediates::Offset32::new(
                    self.offsets.ptr.vmruntime_limits_epoch_deadline() as i32
                ),
            );
        builder.def_var(self.epoch_deadline_var, deadline);
    }

    fn epoch_check(&mut self, builder: &mut FunctionBuilder<'_>) {
        let new_epoch_block = builder.create_block();
        let new_epoch_doublecheck_block = builder.create_block();
        let continuation_block = builder.create_block();
        builder.set_cold_block(new_epoch_block);
        builder.set_cold_block(new_epoch_doublecheck_block);

        let epoch_deadline = builder.use_var(self.epoch_deadline_var);
        // Load new epoch and check against cached deadline. The
        // deadline may be out of date if it was updated (within
        // another yield) during some function that we called; this is
        // fine, as we'll reload it and check again before yielding in
        // the cold path.
        let cur_epoch_value = self.epoch_load_current(builder);
        let cmp = builder.ins().icmp(
            IntCC::UnsignedGreaterThanOrEqual,
            cur_epoch_value,
            epoch_deadline,
        );
        builder
            .ins()
            .brif(cmp, new_epoch_block, &[], continuation_block, &[]);
        builder.seal_block(new_epoch_block);

        // In the "new epoch block", we've noticed that the epoch has
        // exceeded our cached deadline. However the real deadline may
        // have been moved in the meantime. We keep the cached value
        // in a register to speed the checks in the common case
        // (between epoch ticks) but we want to do a precise check
        // here, on the cold path, by reloading the latest value
        // first.
        builder.switch_to_block(new_epoch_block);
        self.epoch_load_deadline_into_var(builder);
        let fresh_epoch_deadline = builder.use_var(self.epoch_deadline_var);
        let fresh_cmp = builder.ins().icmp(
            IntCC::UnsignedGreaterThanOrEqual,
            cur_epoch_value,
            fresh_epoch_deadline,
        );
        builder.ins().brif(
            fresh_cmp,
            new_epoch_doublecheck_block,
            &[],
            continuation_block,
            &[],
        );
        builder.seal_block(new_epoch_doublecheck_block);

        builder.switch_to_block(new_epoch_doublecheck_block);
        let new_epoch_sig = self.builtin_function_signatures.new_epoch(builder.func);
        let (vmctx, new_epoch) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::new_epoch(),
        );
        // new_epoch() returns the new deadline, so we don't have to
        // reload it.
        let call = builder
            .ins()
            .call_indirect(new_epoch_sig, new_epoch, &[vmctx]);
        let new_deadline = *builder.func.dfg.inst_results(call).first().unwrap();
        builder.def_var(self.epoch_deadline_var, new_deadline);
        builder.ins().jump(continuation_block, &[]);
        builder.seal_block(continuation_block);

        builder.switch_to_block(continuation_block);
    }

    fn memory_index_type(&self, index: MemoryIndex) -> ir::Type {
        if self.module.memory_plans[index].memory.memory64 {
            I64
        } else {
            I32
        }
    }

    fn cast_pointer_to_memory_index(
        &self,
        mut pos: FuncCursor<'_>,
        val: ir::Value,
        index: MemoryIndex,
    ) -> ir::Value {
        let desired_type = self.memory_index_type(index);
        let pointer_type = self.pointer_type();
        assert_eq!(pos.func.dfg.value_type(val), pointer_type);

        // The current length is of type `pointer_type` but we need to fit it
        // into `desired_type`. We are guaranteed that the result will always
        // fit, so we just need to do the right ireduce/sextend here.
        if pointer_type == desired_type {
            val
        } else if pointer_type.bits() > desired_type.bits() {
            pos.ins().ireduce(desired_type, val)
        } else {
            // Note that we `sextend` instead of the probably expected
            // `uextend`. This function is only used within the contexts of
            // `memory.size` and `memory.grow` where we're working with units of
            // pages instead of actual bytes, so we know that the upper bit is
            // always cleared for "valid values". The one case we care about
            // sextend would be when the return value of `memory.grow` is `-1`,
            // in which case we want to copy the sign bit.
            //
            // This should only come up on 32-bit hosts running wasm64 modules,
            // which at some point also makes you question various assumptions
            // made along the way...
            pos.ins().sextend(desired_type, val)
        }
    }

    fn cast_memory_index_to_i64(
        &self,
        pos: &mut FuncCursor<'_>,
        val: ir::Value,
        index: MemoryIndex,
    ) -> ir::Value {
        if self.memory_index_type(index) == I64 {
            val
        } else {
            pos.ins().uextend(I64, val)
        }
    }

    fn get_or_init_func_ref_table_elem(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        table: ir::Table,
        index: ir::Value,
    ) -> ir::Value {
        let pointer_type = self.pointer_type();

        // To support lazy initialization of table
        // contents, we check for a null entry here, and
        // if null, we take a slow-path that invokes a
        // libcall.
        let table_entry_addr = builder.ins().table_addr(pointer_type, table, index, 0);
        let flags = ir::MemFlags::trusted().with_table();
        let value = builder.ins().load(pointer_type, flags, table_entry_addr, 0);
        // Mask off the "initialized bit". See documentation on
        // FUNCREF_INIT_BIT in crates/environ/src/ref_bits.rs for more
        // details. Note that `FUNCREF_MASK` has type `usize` which may not be
        // appropriate for the target architecture. Right now its value is
        // always -2 so assert that part doesn't change and then thread through
        // -2 as the immediate.
        assert_eq!(FUNCREF_MASK as isize, -2);
        let value_masked = builder.ins().band_imm(value, Imm64::from(-2));

        let null_block = builder.create_block();
        let continuation_block = builder.create_block();
        let result_param = builder.append_block_param(continuation_block, pointer_type);
        builder.set_cold_block(null_block);

        builder
            .ins()
            .brif(value, continuation_block, &[value_masked], null_block, &[]);
        builder.seal_block(null_block);

        builder.switch_to_block(null_block);
        let table_index = builder.ins().iconst(I32, table_index.index() as i64);
        let builtin_idx = BuiltinFunctionIndex::table_get_lazy_init_func_ref();
        let builtin_sig = self
            .builtin_function_signatures
            .table_get_lazy_init_func_ref(builder.func);
        let (vmctx, builtin_addr) =
            self.translate_load_builtin_function_address(&mut builder.cursor(), builtin_idx);
        let call_inst =
            builder
                .ins()
                .call_indirect(builtin_sig, builtin_addr, &[vmctx, table_index, index]);
        let returned_entry = builder.func.dfg.inst_results(call_inst)[0];
        builder.ins().jump(continuation_block, &[returned_entry]);
        builder.seal_block(continuation_block);

        builder.switch_to_block(continuation_block);
        result_param
    }

    fn check_malloc_start(&mut self, builder: &mut FunctionBuilder) {
        let malloc_start_sig = self.builtin_function_signatures.malloc_start(builder.func);
        let (vmctx, malloc_start) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::malloc_start(),
        );
        builder
            .ins()
            .call_indirect(malloc_start_sig, malloc_start, &[vmctx]);
    }

    fn check_free_start(&mut self, builder: &mut FunctionBuilder) {
        let free_start_sig = self.builtin_function_signatures.free_start(builder.func);
        let (vmctx, free_start) = self.translate_load_builtin_function_address(
            &mut builder.cursor(),
            BuiltinFunctionIndex::free_start(),
        );
        builder
            .ins()
            .call_indirect(free_start_sig, free_start, &[vmctx]);
    }

    fn current_func_name(&self, builder: &mut FunctionBuilder) -> Option<&str> {
        let func_index = match &builder.func.name {
            UserFuncName::User(user) => FuncIndex::from_u32(user.index),
            _ => {
                panic!("function name not a UserFuncName::User as expected")
            }
        };
        self.translation
            .debuginfo
            .name_section
            .func_names
            .get(&func_index)
            .map(|s| *s)
    }
}

struct Call<'a, 'func, 'module_env> {
    builder: &'a mut FunctionBuilder<'func>,
    env: &'a mut FuncEnvironment<'module_env>,
    tail: bool,
}

impl<'a, 'func, 'module_env> Call<'a, 'func, 'module_env> {
    /// Create a new `Call` site that will do regular, non-tail calls.
    pub fn new(
        builder: &'a mut FunctionBuilder<'func>,
        env: &'a mut FuncEnvironment<'module_env>,
    ) -> Self {
        Call {
            builder,
            env,
            tail: false,
        }
    }

    /// Create a new `Call` site that will perform tail calls.
    pub fn new_tail(
        builder: &'a mut FunctionBuilder<'func>,
        env: &'a mut FuncEnvironment<'module_env>,
    ) -> Self {
        Call {
            builder,
            env,
            tail: true,
        }
    }

    /// Do a direct call to the given callee function.
    pub fn direct_call(
        mut self,
        callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        let mut real_call_args = Vec::with_capacity(call_args.len() + 2);
        let caller_vmctx = self
            .builder
            .func
            .special_param(ArgumentPurpose::VMContext)
            .unwrap();

        // Handle direct calls to locally-defined functions.
        if !self.env.module.is_imported_function(callee_index) {
            // First append the callee vmctx address, which is the same as the caller vmctx in
            // this case.
            real_call_args.push(caller_vmctx);

            // Then append the caller vmctx address.
            real_call_args.push(caller_vmctx);

            // Then append the regular call arguments.
            real_call_args.extend_from_slice(call_args);

            // Finally, make the direct call!
            return Ok(self.direct_call_inst(callee, &real_call_args));
        }

        // Handle direct calls to imported functions. We use an indirect call
        // so that we don't have to patch the code at runtime.
        let pointer_type = self.env.pointer_type();
        let sig_ref = self.builder.func.dfg.ext_funcs[callee].signature;
        let vmctx = self.env.vmctx(self.builder.func);
        let base = self.builder.ins().global_value(pointer_type, vmctx);

        let mem_flags = ir::MemFlags::trusted().with_readonly();

        // Load the callee address.
        let body_offset = i32::try_from(
            self.env
                .offsets
                .vmctx_vmfunction_import_wasm_call(callee_index),
        )
        .unwrap();
        let func_addr = self
            .builder
            .ins()
            .load(pointer_type, mem_flags, base, body_offset);

        // First append the callee vmctx address.
        let vmctx_offset =
            i32::try_from(self.env.offsets.vmctx_vmfunction_import_vmctx(callee_index)).unwrap();
        let vmctx = self
            .builder
            .ins()
            .load(pointer_type, mem_flags, base, vmctx_offset);
        real_call_args.push(vmctx);
        real_call_args.push(caller_vmctx);

        // Then append the regular call arguments.
        real_call_args.extend_from_slice(call_args);

        // Finally, make the indirect call!
        Ok(self.indirect_call_inst(sig_ref, func_addr, &real_call_args))
    }

    /// Do an indirect call through the given funcref table.
    pub fn indirect_call(
        mut self,
        table_index: TableIndex,
        table: ir::Table,
        ty_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        let pointer_type = self.env.pointer_type();

        // Get the funcref pointer from the table.
        let funcref_ptr =
            self.env
                .get_or_init_func_ref_table_elem(self.builder, table_index, table, callee);

        // Check for whether the table element is null, and trap if so.
        self.builder
            .ins()
            .trapz(funcref_ptr, ir::TrapCode::IndirectCallToNull);

        // If necessary, check the signature.
        match self.env.module.table_plans[table_index].style {
            TableStyle::CallerChecksSignature => {
                let sig_id_size = self.env.offsets.size_of_vmshared_type_index();
                let sig_id_type = Type::int(u16::from(sig_id_size) * 8).unwrap();
                let vmctx = self.env.vmctx(self.builder.func);
                let base = self.builder.ins().global_value(pointer_type, vmctx);

                // Load the caller ID. This requires loading the `*mut
                // VMFuncRef` base pointer from `VMContext` and then loading,
                // based on `SignatureIndex`, the corresponding entry.
                let mem_flags = ir::MemFlags::trusted().with_readonly();
                let signatures = self.builder.ins().load(
                    pointer_type,
                    mem_flags,
                    base,
                    i32::try_from(self.env.offsets.vmctx_type_ids_array()).unwrap(),
                );
                let sig_index = self.env.module.types[ty_index].unwrap_function();
                let offset =
                    i32::try_from(sig_index.as_u32().checked_mul(sig_id_type.bytes()).unwrap())
                        .unwrap();
                let caller_sig_id =
                    self.builder
                        .ins()
                        .load(sig_id_type, mem_flags, signatures, offset);

                // Load the callee ID.
                let mem_flags = ir::MemFlags::trusted().with_readonly();
                let callee_sig_id = self.builder.ins().load(
                    sig_id_type,
                    mem_flags,
                    funcref_ptr,
                    i32::from(self.env.offsets.ptr.vm_func_ref_type_index()),
                );

                // Check that they match.
                let cmp = self
                    .builder
                    .ins()
                    .icmp(IntCC::Equal, callee_sig_id, caller_sig_id);
                self.builder.ins().trapz(cmp, ir::TrapCode::BadSignature);
            }
        }

        self.unchecked_call(sig_ref, funcref_ptr, call_args)
    }

    /// Call a typed function reference.
    pub fn call_ref(
        mut self,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        // Check for whether the callee is null, and trap if so.
        //
        // FIXME: the wasm type system tracks enough information to know whether
        // `callee` is a null reference or not. In some situations it can be
        // statically known here that `callee` cannot be null in which case this
        // null check can be elided. This requires feeding type information from
        // wasmparser's validator into this function, however, which is not
        // easily done at this time.
        self.builder
            .ins()
            .trapz(callee, ir::TrapCode::NullReference);

        self.unchecked_call(sig_ref, callee, args)
    }

    /// This calls a function by reference without checking the signature.
    ///
    /// It gets the function address, sets relevant flags, and passes the
    /// special callee/caller vmctxs. It is used by both call_indirect (which
    /// checks the signature) and call_ref (which doesn't).
    fn unchecked_call(
        &mut self,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        let pointer_type = self.env.pointer_type();

        // Dereference callee pointer to get the function address.
        let mem_flags = ir::MemFlags::trusted().with_readonly();
        let func_addr = self.builder.ins().load(
            pointer_type,
            mem_flags,
            callee,
            i32::from(self.env.offsets.ptr.vm_func_ref_wasm_call()),
        );

        let mut real_call_args = Vec::with_capacity(call_args.len() + 2);
        let caller_vmctx = self
            .builder
            .func
            .special_param(ArgumentPurpose::VMContext)
            .unwrap();

        // First append the callee vmctx address.
        let vmctx = self.builder.ins().load(
            pointer_type,
            mem_flags,
            callee,
            i32::from(self.env.offsets.ptr.vm_func_ref_vmctx()),
        );
        real_call_args.push(vmctx);
        real_call_args.push(caller_vmctx);

        // Then append the regular call arguments.
        real_call_args.extend_from_slice(call_args);

        Ok(self.indirect_call_inst(sig_ref, func_addr, &real_call_args))
    }

    fn direct_call_inst(&mut self, callee: ir::FuncRef, args: &[ir::Value]) -> ir::Inst {
        if self.tail {
            self.builder.ins().return_call(callee, args)
        } else {
            self.builder.ins().call(callee, args)
        }
    }

    fn indirect_call_inst(
        &mut self,
        sig_ref: ir::SigRef,
        func_addr: ir::Value,
        args: &[ir::Value],
    ) -> ir::Inst {
        if self.tail {
            self.builder
                .ins()
                .return_call_indirect(sig_ref, func_addr, args)
        } else {
            self.builder.ins().call_indirect(sig_ref, func_addr, args)
        }
    }
}

impl TypeConvert for FuncEnvironment<'_> {
    fn lookup_heap_type(&self, ty: wasmparser::UnpackedIndex) -> WasmHeapType {
        wasmtime_environ::WasmparserTypeConverter {
            module: self.module,
            types: self.types,
        }
        .lookup_heap_type(ty)
    }
}

impl<'module_environment> TargetEnvironment for FuncEnvironment<'module_environment> {
    fn target_config(&self) -> TargetFrontendConfig {
        self.isa.frontend_config()
    }

    fn reference_type(&self, ty: WasmHeapType) -> ir::Type {
        crate::reference_type(ty, self.pointer_type())
    }

    fn heap_access_spectre_mitigation(&self) -> bool {
        self.isa.flags().enable_heap_access_spectre_mitigation()
    }

    fn proof_carrying_code(&self) -> bool {
        self.isa.flags().enable_pcc()
    }
}

impl<'module_environment> cranelift_wasm::FuncEnvironment for FuncEnvironment<'module_environment> {
    fn heaps(&self) -> &PrimaryMap<Heap, HeapData> {
        &self.heaps
    }

    fn is_wasm_parameter(&self, _signature: &ir::Signature, index: usize) -> bool {
        // The first two parameters are the vmctx and caller vmctx. The rest are
        // the wasm parameters.
        index >= 2
    }

    fn after_locals(&mut self, num_locals: usize) {
        self.vmruntime_limits_ptr = Variable::new(num_locals);
        self.fuel_var = Variable::new(num_locals + 1);
        self.epoch_deadline_var = Variable::new(num_locals + 2);
        self.epoch_ptr_var = Variable::new(num_locals + 3);
    }

    fn make_table(&mut self, func: &mut ir::Function, index: TableIndex) -> WasmResult<ir::Table> {
        let pointer_type = self.pointer_type();

        let (ptr, base_offset, current_elements_offset) = {
            let vmctx = self.vmctx(func);
            if let Some(def_index) = self.module.defined_table_index(index) {
                let base_offset =
                    i32::try_from(self.offsets.vmctx_vmtable_definition_base(def_index)).unwrap();
                let current_elements_offset = i32::try_from(
                    self.offsets
                        .vmctx_vmtable_definition_current_elements(def_index),
                )
                .unwrap();
                (vmctx, base_offset, current_elements_offset)
            } else {
                let from_offset = self.offsets.vmctx_vmtable_import_from(index);
                let table = func.create_global_value(ir::GlobalValueData::Load {
                    base: vmctx,
                    offset: Offset32::new(i32::try_from(from_offset).unwrap()),
                    global_type: pointer_type,
                    flags: MemFlags::trusted().with_readonly(),
                });
                let base_offset = i32::from(self.offsets.vmtable_definition_base());
                let current_elements_offset =
                    i32::from(self.offsets.vmtable_definition_current_elements());
                (table, base_offset, current_elements_offset)
            }
        };

        let base_gv = func.create_global_value(ir::GlobalValueData::Load {
            base: ptr,
            offset: Offset32::new(base_offset),
            global_type: pointer_type,
            flags: MemFlags::trusted(),
        });
        let bound_gv = func.create_global_value(ir::GlobalValueData::Load {
            base: ptr,
            offset: Offset32::new(current_elements_offset),
            global_type: ir::Type::int(
                u16::from(self.offsets.size_of_vmtable_definition_current_elements()) * 8,
            )
            .unwrap(),
            flags: MemFlags::trusted(),
        });

        let element_size = u64::from(
            self.reference_type(self.module.table_plans[index].table.wasm_ty.heap_type)
                .bytes(),
        );

        Ok(func.create_table(ir::TableData {
            base_gv,
            min_size: Uimm64::new(0),
            bound_gv,
            element_size: Uimm64::new(element_size),
            index_type: I32,
        }))
    }

    fn translate_table_grow(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor<'_>,
        table_index: TableIndex,
        _table: ir::Table,
        delta: ir::Value,
        init_value: ir::Value,
    ) -> WasmResult<ir::Value> {
        let (func_idx, func_sig) =
            match self.module.table_plans[table_index].table.wasm_ty.heap_type {
                WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc => (
                    BuiltinFunctionIndex::table_grow_func_ref(),
                    self.builtin_function_signatures
                        .table_grow_func_ref(&mut pos.func),
                ),
                WasmHeapType::Extern => (
                    BuiltinFunctionIndex::table_grow_externref(),
                    self.builtin_function_signatures
                        .table_grow_externref(&mut pos.func),
                ),
            };

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        let table_index_arg = pos.ins().iconst(I32, table_index.as_u32() as i64);
        let call_inst = pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, table_index_arg, delta, init_value],
        );

        Ok(pos.func.dfg.first_result(call_inst))
    }

    fn translate_table_get(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        table: ir::Table,
        index: ir::Value,
    ) -> WasmResult<ir::Value> {
        let pointer_type = self.pointer_type();

        let plan = &self.module.table_plans[table_index];
        match plan.table.wasm_ty.heap_type {
            WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc => match plan
                .style
            {
                TableStyle::CallerChecksSignature => {
                    Ok(self.get_or_init_func_ref_table_elem(builder, table_index, table, index))
                }
            },
            WasmHeapType::Extern => {
                // Our read barrier for `externref` tables is roughly equivalent
                // to the following pseudocode:
                //
                // ```
                // let elem = table[index]
                // if elem is not null:
                //     let (next, end) = VMExternRefActivationsTable bump region
                //     if next != end:
                //         elem.ref_count += 1
                //         *next = elem
                //         next += 1
                //     else:
                //         call activations_table_insert_with_gc(elem)
                // return elem
                // ```
                //
                // This ensures that all `externref`s coming out of tables and
                // onto the stack are safely held alive by the
                // `VMExternRefActivationsTable`.

                let reference_type = self.reference_type(WasmHeapType::Extern);

                builder.ensure_inserted_block();
                let continue_block = builder.create_block();
                let non_null_elem_block = builder.create_block();
                let gc_block = builder.create_block();
                let no_gc_block = builder.create_block();
                let current_block = builder.current_block().unwrap();
                builder.insert_block_after(non_null_elem_block, current_block);
                builder.insert_block_after(no_gc_block, non_null_elem_block);
                builder.insert_block_after(gc_block, no_gc_block);
                builder.insert_block_after(continue_block, gc_block);

                // Load the table element.
                let elem_addr = builder.ins().table_addr(pointer_type, table, index, 0);
                let flags = ir::MemFlags::trusted().with_table();
                let elem = builder.ins().load(reference_type, flags, elem_addr, 0);

                let elem_is_null = builder.ins().is_null(elem);
                builder
                    .ins()
                    .brif(elem_is_null, continue_block, &[], non_null_elem_block, &[]);

                // Load the `VMExternRefActivationsTable::next` bump finger and
                // the `VMExternRefActivationsTable::end` bump boundary.
                builder.switch_to_block(non_null_elem_block);
                let vmctx = self.vmctx(&mut builder.func);
                let vmctx = builder.ins().global_value(pointer_type, vmctx);
                let activations_table = builder.ins().load(
                    pointer_type,
                    ir::MemFlags::trusted(),
                    vmctx,
                    i32::try_from(self.offsets.vmctx_externref_activations_table()).unwrap(),
                );
                let next = builder.ins().load(
                    pointer_type,
                    ir::MemFlags::trusted(),
                    activations_table,
                    i32::try_from(self.offsets.vm_extern_ref_activation_table_next()).unwrap(),
                );
                let end = builder.ins().load(
                    pointer_type,
                    ir::MemFlags::trusted(),
                    activations_table,
                    i32::try_from(self.offsets.vm_extern_ref_activation_table_end()).unwrap(),
                );

                // If `next == end`, then we are at full capacity. Call a
                // builtin to do a GC and insert this reference into the
                // just-swept table for us.
                let at_capacity = builder.ins().icmp(ir::condcodes::IntCC::Equal, next, end);
                builder
                    .ins()
                    .brif(at_capacity, gc_block, &[], no_gc_block, &[]);
                builder.switch_to_block(gc_block);
                let builtin_idx = BuiltinFunctionIndex::activations_table_insert_with_gc();
                let builtin_sig = self
                    .builtin_function_signatures
                    .activations_table_insert_with_gc(builder.func);
                let (vmctx, builtin_addr) = self
                    .translate_load_builtin_function_address(&mut builder.cursor(), builtin_idx);
                builder
                    .ins()
                    .call_indirect(builtin_sig, builtin_addr, &[vmctx, elem]);
                builder.ins().jump(continue_block, &[]);

                // If `next != end`, then:
                //
                // * increment this reference's ref count,
                // * store the reference into the bump table at `*next`,
                // * and finally increment the `next` bump finger.
                builder.switch_to_block(no_gc_block);
                self.mutate_externref_ref_count(builder, elem, 1);
                builder.ins().store(ir::MemFlags::trusted(), elem, next, 0);

                let new_next = builder
                    .ins()
                    .iadd_imm(next, i64::from(reference_type.bytes()));
                builder.ins().store(
                    ir::MemFlags::trusted(),
                    new_next,
                    activations_table,
                    i32::try_from(self.offsets.vm_extern_ref_activation_table_next()).unwrap(),
                );

                builder.ins().jump(continue_block, &[]);
                builder.switch_to_block(continue_block);

                builder.seal_block(non_null_elem_block);
                builder.seal_block(gc_block);
                builder.seal_block(no_gc_block);
                builder.seal_block(continue_block);

                Ok(elem)
            }
        }
    }

    fn translate_table_set(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        table: ir::Table,
        value: ir::Value,
        index: ir::Value,
    ) -> WasmResult<()> {
        let pointer_type = self.pointer_type();
        let plan = &self.module.table_plans[table_index];
        match plan.table.wasm_ty.heap_type {
            WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc => match plan
                .style
            {
                TableStyle::CallerChecksSignature => {
                    let table_entry_addr = builder.ins().table_addr(pointer_type, table, index, 0);
                    // Set the "initialized bit". See doc-comment on
                    // `FUNCREF_INIT_BIT` in
                    // crates/environ/src/ref_bits.rs for details.
                    let value_with_init_bit = builder
                        .ins()
                        .bor_imm(value, Imm64::from(FUNCREF_INIT_BIT as i64));
                    let flags = ir::MemFlags::trusted().with_table();
                    builder
                        .ins()
                        .store(flags, value_with_init_bit, table_entry_addr, 0);
                    Ok(())
                }
            },

            WasmHeapType::Extern => {
                // Our write barrier for `externref`s being copied out of the
                // stack and into a table is roughly equivalent to the following
                // pseudocode:
                //
                // ```
                // if value != null:
                //     value.ref_count += 1
                // let current_elem = table[index]
                // table[index] = value
                // if current_elem != null:
                //     current_elem.ref_count -= 1
                //     if current_elem.ref_count == 0:
                //         call drop_externref(current_elem)
                // ```
                //
                // This write barrier is responsible for ensuring that:
                //
                // 1. The value's ref count is incremented now that the
                //    table is holding onto it. This is required for memory safety.
                //
                // 2. The old table element, if any, has its ref count
                //    decremented, and that the wrapped data is dropped if the
                //    ref count reaches zero. This is not required for memory
                //    safety, but is required to avoid leaks. Furthermore, the
                //    destructor might GC or touch this table, so we must only
                //    drop the old table element *after* we've replaced it with
                //    the new `value`!

                builder.ensure_inserted_block();
                let current_block = builder.current_block().unwrap();
                let inc_ref_count_block = builder.create_block();
                builder.insert_block_after(inc_ref_count_block, current_block);
                let check_current_elem_block = builder.create_block();
                builder.insert_block_after(check_current_elem_block, inc_ref_count_block);
                let dec_ref_count_block = builder.create_block();
                builder.insert_block_after(dec_ref_count_block, check_current_elem_block);
                let drop_block = builder.create_block();
                builder.insert_block_after(drop_block, dec_ref_count_block);
                let continue_block = builder.create_block();
                builder.insert_block_after(continue_block, drop_block);

                // Calculate the table address of the current element and do
                // bounds checks. This is the first thing we do, because we
                // don't want to modify any ref counts if this `table.set` is
                // going to trap.
                let table_entry_addr = builder.ins().table_addr(pointer_type, table, index, 0);

                // If value is not null, increment `value`'s ref count.
                //
                // This has to come *before* decrementing the current table
                // element's ref count, because it might reach ref count == zero,
                // causing us to deallocate the current table element. However,
                // if `value` *is* the current table element (and therefore this
                // whole `table.set` is a no-op), then we would incorrectly
                // deallocate `value` and leave it in the table, leading to use
                // after free.
                let value_is_null = builder.ins().is_null(value);
                builder.ins().brif(
                    value_is_null,
                    check_current_elem_block,
                    &[],
                    inc_ref_count_block,
                    &[],
                );
                builder.switch_to_block(inc_ref_count_block);
                self.mutate_externref_ref_count(builder, value, 1);
                builder.ins().jump(check_current_elem_block, &[]);

                // Grab the current element from the table, and store the new
                // `value` into the table.
                //
                // Note that we load the current element as a pointer, not a
                // reference. This is so that if we call out-of-line to run its
                // destructor, and its destructor triggers GC, this reference is
                // not recorded in the stack map (which would lead to the GC
                // saving a reference to a deallocated object, and then using it
                // after its been freed).
                builder.switch_to_block(check_current_elem_block);
                let flags = ir::MemFlags::trusted().with_table();
                let current_elem = builder.ins().load(pointer_type, flags, table_entry_addr, 0);
                builder.ins().store(flags, value, table_entry_addr, 0);

                // If the current element is non-null, decrement its reference
                // count. And if its reference count has reached zero, then make
                // an out-of-line call to deallocate it.
                let current_elem_is_null =
                    builder
                        .ins()
                        .icmp_imm(ir::condcodes::IntCC::Equal, current_elem, 0);
                builder.ins().brif(
                    current_elem_is_null,
                    continue_block,
                    &[],
                    dec_ref_count_block,
                    &[],
                );

                builder.switch_to_block(dec_ref_count_block);
                let prev_ref_count = self.mutate_externref_ref_count(builder, current_elem, -1);
                let one = builder.ins().iconst(pointer_type, 1);
                let cond = builder.ins().icmp(IntCC::Equal, one, prev_ref_count);
                builder
                    .ins()
                    .brif(cond, drop_block, &[], continue_block, &[]);

                // Call the `drop_externref` builtin to (you guessed it) drop
                // the `externref`.
                builder.switch_to_block(drop_block);
                let builtin_idx = BuiltinFunctionIndex::drop_externref();
                let builtin_sig = self
                    .builtin_function_signatures
                    .drop_externref(builder.func);
                let (vmctx, builtin_addr) = self
                    .translate_load_builtin_function_address(&mut builder.cursor(), builtin_idx);
                builder
                    .ins()
                    .call_indirect(builtin_sig, builtin_addr, &[vmctx, current_elem]);
                builder.ins().jump(continue_block, &[]);

                builder.switch_to_block(continue_block);

                builder.seal_block(inc_ref_count_block);
                builder.seal_block(check_current_elem_block);
                builder.seal_block(dec_ref_count_block);
                builder.seal_block(drop_block);
                builder.seal_block(continue_block);

                Ok(())
            }
        }
    }

    fn translate_table_fill(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor<'_>,
        table_index: TableIndex,
        dst: ir::Value,
        val: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let (builtin_idx, builtin_sig) =
            match self.module.table_plans[table_index].table.wasm_ty.heap_type {
                WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc => (
                    BuiltinFunctionIndex::table_fill_func_ref(),
                    self.builtin_function_signatures
                        .table_fill_func_ref(&mut pos.func),
                ),
                WasmHeapType::Extern => (
                    BuiltinFunctionIndex::table_fill_externref(),
                    self.builtin_function_signatures
                        .table_fill_externref(&mut pos.func),
                ),
            };

        let (vmctx, builtin_addr) =
            self.translate_load_builtin_function_address(&mut pos, builtin_idx);

        let table_index_arg = pos.ins().iconst(I32, table_index.as_u32() as i64);
        pos.ins().call_indirect(
            builtin_sig,
            builtin_addr,
            &[vmctx, table_index_arg, dst, val, len],
        );

        Ok(())
    }

    fn translate_ref_null(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor,
        ht: WasmHeapType,
    ) -> WasmResult<ir::Value> {
        Ok(match ht {
            WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc => {
                pos.ins().iconst(self.pointer_type(), 0)
            }
            WasmHeapType::Extern => pos.ins().null(self.reference_type(ht)),
        })
    }

    fn translate_ref_is_null(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor,
        value: ir::Value,
    ) -> WasmResult<ir::Value> {
        let bool_is_null = match pos.func.dfg.value_type(value) {
            // `externref`
            ty if ty.is_ref() => pos.ins().is_null(value),
            // `funcref`
            ty if ty == self.pointer_type() => {
                pos.ins()
                    .icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, value, 0)
            }
            _ => unreachable!(),
        };

        Ok(pos.ins().uextend(ir::types::I32, bool_is_null))
    }

    fn translate_ref_func(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor<'_>,
        func_index: FuncIndex,
    ) -> WasmResult<ir::Value> {
        let func_index = pos.ins().iconst(I32, func_index.as_u32() as i64);
        let builtin_index = BuiltinFunctionIndex::ref_func();
        let builtin_sig = self.builtin_function_signatures.ref_func(&mut pos.func);
        let (vmctx, builtin_addr) =
            self.translate_load_builtin_function_address(&mut pos, builtin_index);

        let call_inst = pos
            .ins()
            .call_indirect(builtin_sig, builtin_addr, &[vmctx, func_index]);
        Ok(pos.func.dfg.first_result(call_inst))
    }

    fn translate_custom_global_get(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor<'_>,
        index: cranelift_wasm::GlobalIndex,
    ) -> WasmResult<ir::Value> {
        debug_assert_eq!(
            self.module.globals[index].wasm_ty,
            WasmValType::Ref(WasmRefType::EXTERNREF),
            "We only use GlobalVariable::Custom for externref"
        );

        let builtin_index = BuiltinFunctionIndex::externref_global_get();
        let builtin_sig = self
            .builtin_function_signatures
            .externref_global_get(&mut pos.func);

        let (vmctx, builtin_addr) =
            self.translate_load_builtin_function_address(&mut pos, builtin_index);

        let global_index_arg = pos.ins().iconst(I32, index.as_u32() as i64);
        let call_inst =
            pos.ins()
                .call_indirect(builtin_sig, builtin_addr, &[vmctx, global_index_arg]);

        Ok(pos.func.dfg.first_result(call_inst))
    }

    fn translate_custom_global_set(
        &mut self,
        mut pos: cranelift_codegen::cursor::FuncCursor<'_>,
        index: cranelift_wasm::GlobalIndex,
        value: ir::Value,
    ) -> WasmResult<()> {
        debug_assert_eq!(
            self.module.globals[index].wasm_ty,
            WasmValType::Ref(WasmRefType::EXTERNREF),
            "We only use GlobalVariable::Custom for externref"
        );

        let builtin_index = BuiltinFunctionIndex::externref_global_set();
        let builtin_sig = self
            .builtin_function_signatures
            .externref_global_set(&mut pos.func);

        let (vmctx, builtin_addr) =
            self.translate_load_builtin_function_address(&mut pos, builtin_index);

        let global_index_arg = pos.ins().iconst(I32, index.as_u32() as i64);
        pos.ins()
            .call_indirect(builtin_sig, builtin_addr, &[vmctx, global_index_arg, value]);

        Ok(())
    }

    fn make_heap(&mut self, func: &mut ir::Function, index: MemoryIndex) -> WasmResult<Heap> {
        let pointer_type = self.pointer_type();
        let is_shared = self.module.memory_plans[index].memory.shared;

        let min_size = self.module.memory_plans[index]
            .memory
            .minimum
            .checked_mul(u64::from(WASM_PAGE_SIZE))
            .unwrap_or_else(|| {
                // The only valid Wasm memory size that won't fit in a 64-bit
                // integer is the maximum memory64 size (2^64) which is one
                // larger than `u64::MAX` (2^64 - 1). In this case, just say the
                // minimum heap size is `u64::MAX`.
                debug_assert_eq!(self.module.memory_plans[index].memory.minimum, 1 << 48);
                u64::MAX
            });

        let max_size = self.module.memory_plans[index]
            .memory
            .maximum
            .and_then(|max| max.checked_mul(u64::from(WASM_PAGE_SIZE)));

        let (ptr, base_offset, current_length_offset, ptr_memtype) = {
            let vmctx = self.vmctx(func);
            if let Some(def_index) = self.module.defined_memory_index(index) {
                if is_shared {
                    // As with imported memory, the `VMMemoryDefinition` for a
                    // shared memory is stored elsewhere. We store a `*mut
                    // VMMemoryDefinition` to it and dereference that when
                    // atomically growing it.
                    let from_offset = self.offsets.vmctx_vmmemory_pointer(def_index);
                    let memory = func.create_global_value(ir::GlobalValueData::Load {
                        base: vmctx,
                        offset: Offset32::new(i32::try_from(from_offset).unwrap()),
                        global_type: pointer_type,
                        flags: MemFlags::trusted().with_readonly(),
                    });
                    let base_offset = i32::from(self.offsets.ptr.vmmemory_definition_base());
                    let current_length_offset =
                        i32::from(self.offsets.ptr.vmmemory_definition_current_length());
                    (memory, base_offset, current_length_offset, None)
                } else {
                    let owned_index = self.module.owned_memory_index(def_index);
                    let owned_base_offset =
                        self.offsets.vmctx_vmmemory_definition_base(owned_index);
                    let owned_length_offset = self
                        .offsets
                        .vmctx_vmmemory_definition_current_length(owned_index);
                    let current_base_offset = i32::try_from(owned_base_offset).unwrap();
                    let current_length_offset = i32::try_from(owned_length_offset).unwrap();
                    (
                        vmctx,
                        current_base_offset,
                        current_length_offset,
                        self.pcc_vmctx_memtype,
                    )
                }
            } else {
                let from_offset = self.offsets.vmctx_vmmemory_import_from(index);
                let memory = func.create_global_value(ir::GlobalValueData::Load {
                    base: vmctx,
                    offset: Offset32::new(i32::try_from(from_offset).unwrap()),
                    global_type: pointer_type,
                    flags: MemFlags::trusted().with_readonly(),
                });
                let base_offset = i32::from(self.offsets.ptr.vmmemory_definition_base());
                let current_length_offset =
                    i32::from(self.offsets.ptr.vmmemory_definition_current_length());
                (memory, base_offset, current_length_offset, None)
            }
        };

        // If we have a declared maximum, we can make this a "static" heap, which is
        // allocated up front and never moved.
        let (offset_guard_size, heap_style, readonly_base, base_fact, memory_type) =
            match self.module.memory_plans[index] {
                MemoryPlan {
                    style: MemoryStyle::Dynamic { .. },
                    offset_guard_size,
                    pre_guard_size: _,
                    memory: _,
                } => {
                    let heap_bound = func.create_global_value(ir::GlobalValueData::Load {
                        base: ptr,
                        offset: Offset32::new(current_length_offset),
                        global_type: pointer_type,
                        flags: MemFlags::trusted(),
                    });

                    let (base_fact, data_mt) = if let Some(ptr_memtype) = ptr_memtype {
                        // Create a memtype representing the untyped memory region.
                        let data_mt = func.create_memory_type(ir::MemoryTypeData::DynamicMemory {
                            gv: heap_bound,
                            size: offset_guard_size,
                        });
                        // This fact applies to any pointer to the start of the memory.
                        let base_fact = ir::Fact::dynamic_base_ptr(data_mt);
                        // This fact applies to the length.
                        let length_fact = ir::Fact::global_value(
                            u16::try_from(self.isa.pointer_type().bits()).unwrap(),
                            heap_bound,
                        );
                        // Create a field in the vmctx for the base pointer.
                        match &mut func.memory_types[ptr_memtype] {
                            ir::MemoryTypeData::Struct { size, fields } => {
                                let base_offset = u64::try_from(base_offset).unwrap();
                                fields.push(ir::MemoryTypeField {
                                    offset: base_offset,
                                    ty: self.isa.pointer_type(),
                                    // Read-only field from the PoV of PCC checks:
                                    // don't allow stores to this field. (Even if
                                    // it is a dynamic memory whose base can
                                    // change, that update happens inside the
                                    // runtime, not in generated code.)
                                    readonly: true,
                                    fact: Some(base_fact.clone()),
                                });
                                let current_length_offset =
                                    u64::try_from(current_length_offset).unwrap();
                                fields.push(ir::MemoryTypeField {
                                    offset: current_length_offset,
                                    ty: self.isa.pointer_type(),
                                    // As above, read-only; only the runtime modifies it.
                                    readonly: true,
                                    fact: Some(length_fact),
                                });

                                let pointer_size = u64::from(self.isa.pointer_type().bytes());
                                let fields_end = std::cmp::max(
                                    base_offset + pointer_size,
                                    current_length_offset + pointer_size,
                                );
                                *size = std::cmp::max(*size, fields_end);
                            }
                            _ => {}
                        }
                        // Apply a fact to the base pointer.
                        (Some(base_fact), Some(data_mt))
                    } else {
                        (None, None)
                    };

                    (
                        offset_guard_size,
                        HeapStyle::Dynamic {
                            bound_gv: heap_bound,
                        },
                        false,
                        base_fact,
                        data_mt,
                    )
                }
                MemoryPlan {
                    style: MemoryStyle::Static { bound: bound_pages },
                    offset_guard_size,
                    pre_guard_size: _,
                    memory: _,
                } => {
                    let bound_bytes = u64::from(bound_pages) * u64::from(WASM_PAGE_SIZE);
                    let (base_fact, data_mt) = if let Some(ptr_memtype) = ptr_memtype {
                        // Create a memtype representing the untyped memory region.
                        let data_mt = func.create_memory_type(ir::MemoryTypeData::Memory {
                            size: bound_bytes
                                .checked_add(offset_guard_size)
                                .expect("Memory plan has overflowing size plus guard"),
                        });
                        // This fact applies to any pointer to the start of the memory.
                        let base_fact = Fact::Mem {
                            ty: data_mt,
                            min_offset: 0,
                            max_offset: 0,
                            nullable: false,
                        };
                        // Create a field in the vmctx for the base pointer.
                        match &mut func.memory_types[ptr_memtype] {
                            ir::MemoryTypeData::Struct { size, fields } => {
                                let offset = u64::try_from(base_offset).unwrap();
                                fields.push(ir::MemoryTypeField {
                                    offset,
                                    ty: self.isa.pointer_type(),
                                    // Read-only field from the PoV of PCC checks:
                                    // don't allow stores to this field. (Even if
                                    // it is a dynamic memory whose base can
                                    // change, that update happens inside the
                                    // runtime, not in generated code.)
                                    readonly: true,
                                    fact: Some(base_fact.clone()),
                                });
                                *size = std::cmp::max(
                                    *size,
                                    offset + u64::from(self.isa.pointer_type().bytes()),
                                );
                            }
                            _ => {}
                        }
                        // Apply a fact to the base pointer.
                        (Some(base_fact), Some(data_mt))
                    } else {
                        (None, None)
                    };
                    (
                        offset_guard_size,
                        HeapStyle::Static { bound: bound_bytes },
                        true,
                        base_fact,
                        data_mt,
                    )
                }
            };

        let mut flags = MemFlags::trusted().with_checked();
        if readonly_base {
            flags.set_readonly();
        }
        let heap_base = func.create_global_value(ir::GlobalValueData::Load {
            base: ptr,
            offset: Offset32::new(base_offset),
            global_type: pointer_type,
            flags,
        });
        func.global_value_facts[heap_base] = base_fact;

        Ok(self.heaps.push(HeapData {
            base: heap_base,
            min_size,
            max_size,
            offset_guard_size,
            style: heap_style,
            index_type: self.memory_index_type(index),
            memory_type,
        }))
    }

    fn make_global(
        &mut self,
        func: &mut ir::Function,
        index: GlobalIndex,
    ) -> WasmResult<GlobalVariable> {
        let ty = self.module.globals[index].wasm_ty;
        match ty {
            // Although `ExternRef`s live at the same memory location as any
            // other type of global at the same index would, getting or setting
            // them requires ref counting barriers. Therefore, we need to use
            // `GlobalVariable::Custom`, as that is the only kind of
            // `GlobalVariable` for which `cranelift-wasm` supports custom
            // access translation.
            WasmValType::Ref(WasmRefType {
                heap_type: WasmHeapType::Extern,
                ..
            }) => return Ok(GlobalVariable::Custom),

            // Funcrefs are represented as pointers which survive for the
            // entire lifetime of the `Store` so there's no need for barriers.
            // This means that they can fall through to memory as well.
            WasmValType::Ref(WasmRefType {
                heap_type: WasmHeapType::Func | WasmHeapType::Concrete(_) | WasmHeapType::NoFunc,
                ..
            }) => {}

            // Value types all live in memory so let them fall through to a
            // memory-based global.
            WasmValType::I32
            | WasmValType::I64
            | WasmValType::F32
            | WasmValType::F64
            | WasmValType::V128 => {}
        }

        let (gv, offset) = self.get_global_location(func, index);
        Ok(GlobalVariable::Memory {
            gv,
            offset: offset.into(),
            ty: super::value_type(self.isa, ty),
        })
    }

    fn make_indirect_sig(
        &mut self,
        func: &mut ir::Function,
        index: TypeIndex,
    ) -> WasmResult<ir::SigRef> {
        let index = self.module.types[index].unwrap_function();
        let sig = crate::wasm_call_signature(self.isa, &self.types[index], &self.tunables);
        Ok(func.import_signature(sig))
    }

    fn make_direct_func(
        &mut self,
        func: &mut ir::Function,
        index: FuncIndex,
    ) -> WasmResult<ir::FuncRef> {
        let sig = self.module.functions[index].signature;
        let sig = crate::wasm_call_signature(self.isa, &self.types[sig], &self.tunables);
        let signature = func.import_signature(sig);
        let name =
            ir::ExternalName::User(func.declare_imported_user_function(ir::UserExternalName {
                namespace: 0,
                index: index.as_u32(),
            }));
        Ok(func.import_function(ir::ExtFuncData {
            name,
            signature,

            // The value of this flag determines the codegen for calls to this
            // function. If this flag is `false` then absolute relocations will
            // be generated for references to the function, which requires
            // load-time relocation resolution. If this flag is set to `true`
            // then relative relocations are emitted which can be resolved at
            // object-link-time, just after all functions are compiled.
            //
            // This flag is set to `true` for functions defined in the object
            // we'll be defining in this compilation unit, or everything local
            // to the wasm module. This means that between functions in a wasm
            // module there's relative calls encoded. All calls external to a
            // wasm module (e.g. imports or libcalls) are either encoded through
            // the `VMContext` as relative jumps (hence no relocations) or
            // they're libcalls with absolute relocations.
            colocated: self.module.defined_func_index(index).is_some(),
        }))
    }

    fn translate_call_indirect(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        table: ir::Table,
        ty_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        Call::new(builder, self).indirect_call(
            table_index,
            table,
            ty_index,
            sig_ref,
            callee,
            call_args,
        )
    }

    fn translate_call(
        &mut self,
        builder: &mut FunctionBuilder,
        callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        Call::new(builder, self).direct_call(callee_index, callee, call_args)
    }

    fn translate_call_ref(
        &mut self,
        builder: &mut FunctionBuilder,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<ir::Inst> {
        Call::new(builder, self).call_ref(sig_ref, callee, call_args)
    }

    fn translate_return_call(
        &mut self,
        builder: &mut FunctionBuilder,
        callee_index: FuncIndex,
        callee: ir::FuncRef,
        call_args: &[ir::Value],
    ) -> WasmResult<()> {
        Call::new_tail(builder, self).direct_call(callee_index, callee, call_args)?;
        Ok(())
    }

    fn translate_return_call_indirect(
        &mut self,
        builder: &mut FunctionBuilder,
        table_index: TableIndex,
        table: ir::Table,
        ty_index: TypeIndex,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<()> {
        Call::new_tail(builder, self).indirect_call(
            table_index,
            table,
            ty_index,
            sig_ref,
            callee,
            call_args,
        )?;
        Ok(())
    }

    fn translate_return_call_ref(
        &mut self,
        builder: &mut FunctionBuilder,
        sig_ref: ir::SigRef,
        callee: ir::Value,
        call_args: &[ir::Value],
    ) -> WasmResult<()> {
        Call::new_tail(builder, self).call_ref(sig_ref, callee, call_args)?;
        Ok(())
    }

    fn translate_memory_grow(
        &mut self,
        mut pos: FuncCursor<'_>,
        index: MemoryIndex,
        _heap: Heap,
        val: ir::Value,
    ) -> WasmResult<ir::Value> {
        let func_sig = self
            .builtin_function_signatures
            .memory32_grow(&mut pos.func);
        let index_arg = index.index();

        let memory_index = pos.ins().iconst(I32, index_arg as i64);
        let (vmctx, func_addr) = self.translate_load_builtin_function_address(
            &mut pos,
            BuiltinFunctionIndex::memory32_grow(),
        );

        let val = self.cast_memory_index_to_i64(&mut pos, val, index);
        let call_inst = pos
            .ins()
            .call_indirect(func_sig, func_addr, &[vmctx, val, memory_index]);
        let result = *pos.func.dfg.inst_results(call_inst).first().unwrap();
        Ok(self.cast_pointer_to_memory_index(pos, result, index))
    }

    fn translate_memory_size(
        &mut self,
        mut pos: FuncCursor<'_>,
        index: MemoryIndex,
        _heap: Heap,
    ) -> WasmResult<ir::Value> {
        let pointer_type = self.pointer_type();
        let vmctx = self.vmctx(&mut pos.func);
        let is_shared = self.module.memory_plans[index].memory.shared;
        let base = pos.ins().global_value(pointer_type, vmctx);
        let current_length_in_bytes = match self.module.defined_memory_index(index) {
            Some(def_index) => {
                if is_shared {
                    let offset =
                        i32::try_from(self.offsets.vmctx_vmmemory_pointer(def_index)).unwrap();
                    let vmmemory_ptr =
                        pos.ins()
                            .load(pointer_type, ir::MemFlags::trusted(), base, offset);
                    let vmmemory_definition_offset =
                        i64::from(self.offsets.ptr.vmmemory_definition_current_length());
                    let vmmemory_definition_ptr =
                        pos.ins().iadd_imm(vmmemory_ptr, vmmemory_definition_offset);
                    // This atomic access of the
                    // `VMMemoryDefinition::current_length` is direct; no bounds
                    // check is needed. This is possible because shared memory
                    // has a static size (the maximum is always known). Shared
                    // memory is thus built with a static memory plan and no
                    // bounds-checked version of this is implemented.
                    pos.ins().atomic_load(
                        pointer_type,
                        ir::MemFlags::trusted(),
                        vmmemory_definition_ptr,
                    )
                } else {
                    let owned_index = self.module.owned_memory_index(def_index);
                    let offset = i32::try_from(
                        self.offsets
                            .vmctx_vmmemory_definition_current_length(owned_index),
                    )
                    .unwrap();
                    pos.ins()
                        .load(pointer_type, ir::MemFlags::trusted(), base, offset)
                }
            }
            None => {
                let offset = i32::try_from(self.offsets.vmctx_vmmemory_import_from(index)).unwrap();
                let vmmemory_ptr =
                    pos.ins()
                        .load(pointer_type, ir::MemFlags::trusted(), base, offset);
                if is_shared {
                    let vmmemory_definition_offset =
                        i64::from(self.offsets.ptr.vmmemory_definition_current_length());
                    let vmmemory_definition_ptr =
                        pos.ins().iadd_imm(vmmemory_ptr, vmmemory_definition_offset);
                    pos.ins().atomic_load(
                        pointer_type,
                        ir::MemFlags::trusted(),
                        vmmemory_definition_ptr,
                    )
                } else {
                    pos.ins().load(
                        pointer_type,
                        ir::MemFlags::trusted(),
                        vmmemory_ptr,
                        i32::from(self.offsets.ptr.vmmemory_definition_current_length()),
                    )
                }
            }
        };
        let current_length_in_pages = pos
            .ins()
            .udiv_imm(current_length_in_bytes, i64::from(WASM_PAGE_SIZE));

        Ok(self.cast_pointer_to_memory_index(pos, current_length_in_pages, index))
    }

    fn translate_memory_copy(
        &mut self,
        mut pos: FuncCursor,
        src_index: MemoryIndex,
        _src_heap: Heap,
        dst_index: MemoryIndex,
        _dst_heap: Heap,
        dst: ir::Value,
        src: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let (vmctx, func_addr) = self
            .translate_load_builtin_function_address(&mut pos, BuiltinFunctionIndex::memory_copy());

        let func_sig = self.builtin_function_signatures.memory_copy(&mut pos.func);
        let dst = self.cast_memory_index_to_i64(&mut pos, dst, dst_index);
        let src = self.cast_memory_index_to_i64(&mut pos, src, src_index);
        // The length is 32-bit if either memory is 32-bit, but if they're both
        // 64-bit then it's 64-bit. Our intrinsic takes a 64-bit length for
        // compatibility across all memories, so make sure that it's cast
        // correctly here (this is a bit special so no generic helper unlike for
        // `dst`/`src` above)
        let len = if self.memory_index_type(dst_index) == I64
            && self.memory_index_type(src_index) == I64
        {
            len
        } else {
            pos.ins().uextend(I64, len)
        };
        let src_index = pos.ins().iconst(I32, i64::from(src_index.as_u32()));
        let dst_index = pos.ins().iconst(I32, i64::from(dst_index.as_u32()));
        pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, dst_index, dst, src_index, src, len],
        );

        Ok(())
    }

    fn translate_memory_fill(
        &mut self,
        mut pos: FuncCursor,
        memory_index: MemoryIndex,
        _heap: Heap,
        dst: ir::Value,
        val: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let func_sig = self.builtin_function_signatures.memory_fill(&mut pos.func);
        let dst = self.cast_memory_index_to_i64(&mut pos, dst, memory_index);
        let len = self.cast_memory_index_to_i64(&mut pos, len, memory_index);
        let memory_index_arg = pos.ins().iconst(I32, i64::from(memory_index.as_u32()));

        let (vmctx, func_addr) = self
            .translate_load_builtin_function_address(&mut pos, BuiltinFunctionIndex::memory_fill());

        pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, memory_index_arg, dst, val, len],
        );

        Ok(())
    }

    fn translate_memory_init(
        &mut self,
        mut pos: FuncCursor,
        memory_index: MemoryIndex,
        _heap: Heap,
        seg_index: u32,
        dst: ir::Value,
        src: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let (func_sig, func_idx) = self.get_memory_init_func(&mut pos.func);

        let memory_index_arg = pos.ins().iconst(I32, memory_index.index() as i64);
        let seg_index_arg = pos.ins().iconst(I32, seg_index as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        let dst = self.cast_memory_index_to_i64(&mut pos, dst, memory_index);

        pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, memory_index_arg, seg_index_arg, dst, src, len],
        );

        Ok(())
    }

    fn translate_data_drop(&mut self, mut pos: FuncCursor, seg_index: u32) -> WasmResult<()> {
        let (func_sig, func_idx) = self.get_data_drop_func(&mut pos.func);
        let seg_index_arg = pos.ins().iconst(I32, seg_index as i64);
        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);
        pos.ins()
            .call_indirect(func_sig, func_addr, &[vmctx, seg_index_arg]);
        Ok(())
    }

    fn translate_table_size(
        &mut self,
        mut pos: FuncCursor,
        _table_index: TableIndex,
        table: ir::Table,
    ) -> WasmResult<ir::Value> {
        let size_gv = pos.func.tables[table].bound_gv;
        Ok(pos.ins().global_value(ir::types::I32, size_gv))
    }

    fn translate_table_copy(
        &mut self,
        mut pos: FuncCursor,
        dst_table_index: TableIndex,
        _dst_table: ir::Table,
        src_table_index: TableIndex,
        _src_table: ir::Table,
        dst: ir::Value,
        src: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let (func_sig, dst_table_index_arg, src_table_index_arg, func_idx) =
            self.get_table_copy_func(&mut pos.func, dst_table_index, src_table_index);

        let dst_table_index_arg = pos.ins().iconst(I32, dst_table_index_arg as i64);
        let src_table_index_arg = pos.ins().iconst(I32, src_table_index_arg as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[
                vmctx,
                dst_table_index_arg,
                src_table_index_arg,
                dst,
                src,
                len,
            ],
        );

        Ok(())
    }

    fn translate_table_init(
        &mut self,
        mut pos: FuncCursor,
        seg_index: u32,
        table_index: TableIndex,
        _table: ir::Table,
        dst: ir::Value,
        src: ir::Value,
        len: ir::Value,
    ) -> WasmResult<()> {
        let (func_sig, table_index_arg, func_idx) =
            self.get_table_init_func(&mut pos.func, table_index);

        let table_index_arg = pos.ins().iconst(I32, table_index_arg as i64);
        let seg_index_arg = pos.ins().iconst(I32, seg_index as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, table_index_arg, seg_index_arg, dst, src, len],
        );

        Ok(())
    }

    fn translate_elem_drop(&mut self, mut pos: FuncCursor, elem_index: u32) -> WasmResult<()> {
        let (func_sig, func_idx) = self.get_elem_drop_func(&mut pos.func);

        let elem_index_arg = pos.ins().iconst(I32, elem_index as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        pos.ins()
            .call_indirect(func_sig, func_addr, &[vmctx, elem_index_arg]);

        Ok(())
    }

    fn translate_atomic_wait(
        &mut self,
        mut pos: FuncCursor,
        memory_index: MemoryIndex,
        _heap: Heap,
        addr: ir::Value,
        expected: ir::Value,
        timeout: ir::Value,
    ) -> WasmResult<ir::Value> {
        let addr = self.cast_memory_index_to_i64(&mut pos, addr, memory_index);
        let implied_ty = pos.func.dfg.value_type(expected);
        let (func_sig, memory_index, func_idx) =
            self.get_memory_atomic_wait(&mut pos.func, memory_index, implied_ty);

        let memory_index_arg = pos.ins().iconst(I32, memory_index as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(&mut pos, func_idx);

        let call_inst = pos.ins().call_indirect(
            func_sig,
            func_addr,
            &[vmctx, memory_index_arg, addr, expected, timeout],
        );

        Ok(*pos.func.dfg.inst_results(call_inst).first().unwrap())
    }

    fn translate_atomic_notify(
        &mut self,
        mut pos: FuncCursor,
        memory_index: MemoryIndex,
        _heap: Heap,
        addr: ir::Value,
        count: ir::Value,
    ) -> WasmResult<ir::Value> {
        let addr = self.cast_memory_index_to_i64(&mut pos, addr, memory_index);
        let func_sig = self
            .builtin_function_signatures
            .memory_atomic_notify(&mut pos.func);

        let memory_index_arg = pos.ins().iconst(I32, memory_index.index() as i64);

        let (vmctx, func_addr) = self.translate_load_builtin_function_address(
            &mut pos,
            BuiltinFunctionIndex::memory_atomic_notify(),
        );

        let call_inst =
            pos.ins()
                .call_indirect(func_sig, func_addr, &[vmctx, memory_index_arg, addr, count]);

        Ok(*pos.func.dfg.inst_results(call_inst).first().unwrap())
    }

    fn translate_loop_header(&mut self, builder: &mut FunctionBuilder) -> WasmResult<()> {
        // Additionally if enabled check how much fuel we have remaining to see
        // if we've run out by this point.
        if self.tunables.consume_fuel {
            self.fuel_check(builder);
        }

        // If we are performing epoch-based interruption, check to see
        // if the epoch counter has changed.
        if self.tunables.epoch_interruption {
            self.epoch_check(builder);
        }

        Ok(())
    }

    fn before_translate_operator(
        &mut self,
        op: &Operator,
        builder: &mut FunctionBuilder,
        state: &FuncTranslationState,
    ) -> WasmResult<()> {
        if self.tunables.consume_fuel {
            self.fuel_before_op(op, builder, state.reachable());
        }
        Ok(())
    }

    fn after_translate_operator(
        &mut self,
        op: &Operator,
        builder: &mut FunctionBuilder,
        state: &FuncTranslationState,
    ) -> WasmResult<()> {
        if self.tunables.consume_fuel && state.reachable() {
            self.fuel_after_op(op, builder);
        }
        Ok(())
    }

    fn before_unconditionally_trapping_memory_access(
        &mut self,
        builder: &mut FunctionBuilder,
    ) -> WasmResult<()> {
        if self.tunables.consume_fuel {
            self.fuel_increment_var(builder);
            self.fuel_save_from_var(builder);
        }
        Ok(())
    }

    fn before_translate_function(
        &mut self,
        builder: &mut FunctionBuilder,
        _state: &FuncTranslationState,
    ) -> WasmResult<()> {
        // If the `vmruntime_limits_ptr` variable will get used then we initialize
        // it here.
        if self.tunables.consume_fuel || self.tunables.epoch_interruption {
            self.declare_vmruntime_limits_ptr(builder);
        }
        // Additionally we initialize `fuel_var` if it will get used.
        if self.tunables.consume_fuel {
            self.fuel_function_entry(builder);
        }
        // Initialize `epoch_var` with the current epoch.
        if self.tunables.epoch_interruption {
            self.epoch_function_entry(builder);
        }

        let func_name = self.current_func_name(builder);
        if func_name == Some("malloc") {
            self.check_malloc_start(builder);
        } else if func_name == Some("free") {
            self.check_free_start(builder);
        }

        Ok(())
    }

    fn after_translate_function(
        &mut self,
        builder: &mut FunctionBuilder,
        state: &FuncTranslationState,
    ) -> WasmResult<()> {
        if self.tunables.consume_fuel && state.reachable() {
            self.fuel_function_exit(builder);
        }
        if let Some(pcc_vmctx_memtype) = self.pcc_vmctx_memtype {
            // Sort the fields by offset in the struct definition for
            // vmctx, now that we've completed it.
            match &mut builder.func.memory_types[pcc_vmctx_memtype] {
                ir::MemoryTypeData::Struct { fields, .. } => {
                    fields.sort_by_key(|f| f.offset);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn relaxed_simd_deterministic(&self) -> bool {
        self.tunables.relaxed_simd_deterministic
    }

    fn has_native_fma(&self) -> bool {
        self.isa.has_native_fma()
    }

    fn is_x86(&self) -> bool {
        self.isa.triple().architecture == target_lexicon::Architecture::X86_64
    }

    fn use_x86_blendv_for_relaxed_laneselect(&self, ty: Type) -> bool {
        self.isa.has_x86_blendv_lowering(ty)
    }

    fn use_x86_pshufb_for_relaxed_swizzle(&self) -> bool {
        self.isa.has_x86_pshufb_lowering()
    }

    fn use_x86_pmulhrsw_for_relaxed_q15mul(&self) -> bool {
        self.isa.has_x86_pmulhrsw_lowering()
    }

    fn use_x86_pmaddubsw_for_dot(&self) -> bool {
        self.isa.has_x86_pmaddubsw_lowering()
    }

    cfg_if! {
        if #[cfg(feature = "wmemcheck")] {
            fn handle_before_return(
                &mut self,
                retvals: &[Value],
                builder: &mut FunctionBuilder,
            ) {
                if self.wmemcheck {
                    let func_name = self.current_func_name(builder);
                    if func_name == Some("malloc") {
                        self.hook_malloc_exit(builder, retvals);
                    } else if func_name == Some("free") {
                        self.hook_free_exit(builder);
                    }
                }
            }

            fn before_load(&mut self, builder: &mut FunctionBuilder, val_size: u8, addr: ir::Value, offset: u64) {
                if self.wmemcheck {
                    let check_load_sig = self.builtin_function_signatures.check_load(builder.func);
                    let (vmctx, check_load) = self.translate_load_builtin_function_address(
                        &mut builder.cursor(),
                        BuiltinFunctionIndex::check_load(),
                    );
                    let num_bytes = builder.ins().iconst(I32, val_size as i64);
                    let offset_val = builder.ins().iconst(I64, offset as i64);
                    builder
                        .ins()
                        .call_indirect(check_load_sig, check_load, &[vmctx, num_bytes, addr, offset_val]);
                }
            }

            fn before_store(&mut self, builder: &mut FunctionBuilder, val_size: u8, addr: ir::Value, offset: u64) {
                if self.wmemcheck {
                    let check_store_sig = self.builtin_function_signatures.check_store(builder.func);
                    let (vmctx, check_store) = self.translate_load_builtin_function_address(
                        &mut builder.cursor(),
                        BuiltinFunctionIndex::check_store(),
                    );
                    let num_bytes = builder.ins().iconst(I32, val_size as i64);
                    let offset_val = builder.ins().iconst(I64, offset as i64);
                    builder
                        .ins()
                        .call_indirect(check_store_sig, check_store, &[vmctx, num_bytes, addr, offset_val]);
                }
            }

            fn update_global(&mut self, builder: &mut FunctionBuilder, global_index: u32, value: ir::Value) {
                if self.wmemcheck {
                    if global_index == 0 {
                        // We are making the assumption that global 0 is the auxiliary stack pointer.
                        let update_stack_pointer_sig = self.builtin_function_signatures.update_stack_pointer(builder.func);
                        let (vmctx, update_stack_pointer) = self.translate_load_builtin_function_address(
                            &mut builder.cursor(),
                            BuiltinFunctionIndex::update_stack_pointer(),
                        );
                        builder
                            .ins()
                            .call_indirect(update_stack_pointer_sig, update_stack_pointer, &[vmctx, value]);
                    }
                }
            }

            fn before_memory_grow(&mut self, builder: &mut FunctionBuilder, num_pages: ir::Value, mem_index: MemoryIndex) {
                if self.wmemcheck && mem_index.as_u32() == 0 {
                    let update_mem_size_sig = self.builtin_function_signatures.update_mem_size(builder.func);
                    let (vmctx, update_mem_size) = self.translate_load_builtin_function_address(
                        &mut builder.cursor(),
                        BuiltinFunctionIndex::update_mem_size(),
                    );
                    builder
                        .ins()
                        .call_indirect(update_mem_size_sig, update_mem_size, &[vmctx, num_pages]);
                }
            }
        } else {
            fn handle_before_return(&mut self, _retvals: &[Value], builder: &mut FunctionBuilder) {
                let _ = self.builtin_function_signatures.check_malloc(builder.func);
                let _ = self.builtin_function_signatures.check_free(builder.func);
            }

            fn before_load(&mut self, builder: &mut FunctionBuilder, _val_size: u8, _addr: ir::Value, _offset: u64) {
                let _ = self.builtin_function_signatures.check_load(builder.func);
            }

            fn before_store(&mut self, builder: &mut FunctionBuilder, _val_size: u8, _addr: ir::Value, _offset: u64) {
                let _ = self.builtin_function_signatures.check_store(builder.func);
            }

            fn update_global(&mut self, builder: &mut FunctionBuilder, _global_index: u32, _value: ir::Value) {
                let _ = self.builtin_function_signatures.update_stack_pointer(builder.func);
            }

            fn before_memory_grow(&mut self, builder: &mut FunctionBuilder, _num_pages: Value, _mem_index: MemoryIndex) {
                let _ = self.builtin_function_signatures.update_mem_size(builder.func);
            }
        }
    }
}
