use rustc::hir::def_id::DefId;
use rustc::mir;
use rustc::ty::{self, TypeVariants, Ty};
use rustc::ty::layout::HasDataLayout;
use syntax::codemap::Span;
use syntax::attr;
use rustc_target::spec::abi::Abi;

use constraints::Constraint;
use error::{EvalError, EvalResult};
use eval_context::{EvalContext, StackPopCleanup, ValTy, is_inhabited};
use executor::{FinishStep, FinishStepVariant};
use place::Place;
use memory::{SByte};
use value::{PrimVal, PrimValKind};
use value::Value;
use rustc_data_structures::indexed_vec::Idx;

mod drop;
mod intrinsic;

impl<'a, 'tcx> EvalContext<'a, 'tcx> {
    pub(super) fn goto_block(&mut self, target: mir::BasicBlock) {
        self.frame_mut().block = target;
        self.frame_mut().stmt = 0;
    }

    // If the result is a branch on an abstract discriminant, returns a vector
    // of the possible branches. Otherwise just takes the step and returns None.
    pub(super) fn eval_terminator(
        &mut self,
        terminator: &mir::Terminator<'tcx>,
    ) -> EvalResult<'tcx, Option<Vec<FinishStep<'tcx>>>> {
        use rustc::mir::TerminatorKind::*;
        match terminator.kind {
            Return => {
                self.dump_local(self.frame().return_place);
                self.pop_stack_frame()?;
                Ok(None)
            }

            Goto { target } => {
                self.goto_block(target);
                Ok(None)
            },

            SwitchInt { ref discr, ref values, ref targets, .. } => {
                let discr_val = self.eval_operand(discr)?;
                let discr_ty = self.operand_ty(discr);
                let discr_prim = self.value_to_primval(discr_val, discr_ty)?;
                let discr_kind = self.ty_to_primval_kind(discr_ty)?;

                if discr_prim.is_concrete() {

                    // Branch to the `otherwise` case by default, if no match is found.
                    let mut target_block = targets[targets.len() - 1];

                    for (index, const_int) in values.iter().enumerate() {
                        let prim = PrimVal::Bytes(*const_int);
                        if discr_prim.to_bytes()? == prim.to_bytes()? {
                            target_block = targets[index];
                            break;
                        }
                    }

                    self.goto_block(target_block);
                    Ok(None)
                } else {
                    let mut feasible_blocks_with_constraints = Vec::new();
                    let mut otherwise_constraints = Vec::new();
                    for (index, const_int) in values.iter().enumerate() {
                        let prim = PrimVal::Bytes(*const_int);
                        let eq_constraint = Constraint::new_compare(
                            mir::BinOp::Eq, discr_kind, discr_prim, prim);
                        otherwise_constraints.push(
                            Constraint::new_compare(mir::BinOp::Ne, discr_kind, discr_prim, prim));
                        if self.memory.constraints.is_feasible_with(&[eq_constraint]) {
                            feasible_blocks_with_constraints.push(
                                FinishStep {
                                    constraints: vec![eq_constraint],
                                    variant: FinishStepVariant::Continue {
                                        goto_block: targets[index],
                                        set_place: None,
                                    },
                                });
                        }
                    }

                    if self.memory.constraints.is_feasible_with(&otherwise_constraints) {
                        feasible_blocks_with_constraints.push(
                            FinishStep {
                                constraints: otherwise_constraints,
                                variant: FinishStepVariant::Continue {
                                    goto_block: targets[targets.len() - 1],
                                    set_place: None,
                                }
                            });
                    }

                    Ok(Some(feasible_blocks_with_constraints))
                }
            }

            Call { ref func, ref args, ref destination, .. } => {
                let destination = match *destination {
                    Some((ref lv, target)) => Some((self.eval_place(lv)?, target)),
                    None => None,
                };

                let func_ty = self.operand_ty(func);
                let (fn_def, sig) = match func_ty.sty {
                    ty::TyFnPtr(sig) => {
                        let fn_ptr = self.eval_operand_to_primval(func)?.to_ptr()?;
                        let instance = self.memory.get_fn(fn_ptr)?;
                        let instance_ty = instance.ty(self.tcx);
                        let instance_ty = self.monomorphize(instance_ty, instance.substs);
                        match instance_ty.sty {
                            ty::TyFnDef(..) => {
                                let real_sig = instance_ty.fn_sig(self.tcx);
                                let sig = self.tcx.normalize_erasing_late_bound_regions(
                                    ty::ParamEnv::reveal_all(),
                                    &sig,
                                );
                                let real_sig = self.tcx.normalize_erasing_late_bound_regions(
                                    ty::ParamEnv::reveal_all(),
                                    &real_sig,
                                );
                                if !self.check_sig_compat(sig, real_sig)? {
                                    return Err(EvalError::FunctionPointerTyMismatch(real_sig, sig));
                                }
                            },
                            ref other => bug!("instance def ty: {:?}", other),
                        }
                        (instance, sig)
                    },
                    ty::TyFnDef(def_id, substs) => (self.resolve(def_id, substs)?, func_ty.fn_sig(self.tcx)),
                    _ => {
                        let msg = format!("can't handle callee of type {:?}", func_ty);
                        return Err(EvalError::Unimplemented(msg));
                    }
                };
                let sig = self.erase_lifetimes(&sig);
                self.eval_fn_call(fn_def, destination, args, terminator.source_info.span, sig)
            }

            Drop { ref location, target, .. } => {
                trace!("TerminatorKind::drop: {:?}, {:?}", location, self.substs());
                let lval = self.eval_place(location)?;
                let ty = self.place_ty(location);
                self.goto_block(target);
                let ty = ::eval_context::apply_param_substs(self.tcx, self.substs(), &ty);

                let instance = ::eval_context::resolve_drop_in_place(self.tcx, ty);
                self.drop_place(lval, instance, ty, terminator.source_info.span)?;
                Ok(None)
            }

            Assert { ref cond, expected, ref msg, target, .. } => {
                let cond_val = self.eval_operand_to_primval(cond)?;
                if cond_val.is_concrete() {
                    let cond_val = cond_val.to_bool()?;
                    if expected == cond_val {
                        self.goto_block(target);
                        Ok(None)
                    } else {
                        use rustc::mir::interpret::EvalErrorKind::*;
                        match *msg {
                            BoundsCheck { ref len, ref index } => {
                                let span = terminator.source_info.span;
                                let len = self.eval_operand_to_primval(len)
                                    .expect("can't eval len")
                                    .to_u64()?;
                                let index = self.eval_operand_to_primval(index)
                                    .expect("can't eval index")
                                    .to_u64()?;
                                Err(EvalError::ArrayIndexOutOfBounds(span, len, index))
                            }
                            Overflow(op) => Err(EvalError::Overflow(op)),
                            OverflowNeg => Err(EvalError::OverflowNeg),
                            RemainderByZero => Err(EvalError::RemainderByZero),
                            _ => unimplemented!(),
                        }
                    }
                } else {
                    let expected_val = PrimVal::from_bool(expected);
                    let succeed_constraints = vec![
                        Constraint::new_compare(
                            mir::BinOp::Eq, PrimValKind::Bool, cond_val, expected_val)];

                    let fail_constraints = vec![
                        Constraint::new_compare(
                            mir::BinOp::Ne, PrimValKind::Bool, cond_val, expected_val)];

                    let mut finish_steps = Vec::new();

                    if self.memory.constraints.is_feasible_with(&succeed_constraints[..]) {
                        finish_steps.push(
                            FinishStep {
                                constraints: succeed_constraints,
                                variant: FinishStepVariant::Continue {
                                    goto_block: target,
                                    set_place: None,
                                },
                            });
                    }

                    if self.memory.constraints.is_feasible_with(&fail_constraints[..]) {
                        use rustc::mir::interpret::EvalErrorKind::*;
                        let e = match *msg {
                            BoundsCheck { ref len, ref index } => {
                                let span = terminator.source_info.span;
                                let len = self.eval_operand_to_primval(len)
                                    .expect("can't eval len")
                                    .to_u64()?;
                                let index = self.eval_operand_to_primval(index)
                                    .expect("can't eval index")
                                    .to_u64()?;
                                EvalError::ArrayIndexOutOfBounds(span, len, index)
                            },
                            _ => unimplemented!(),
                        };

                        finish_steps.push(
                            FinishStep {
                                constraints: fail_constraints,
                                variant: FinishStepVariant::Error(e),
                            });
                    }

                    Ok(Some(finish_steps))
                }
            },

            DropAndReplace { .. } => unimplemented!(),
            Resume => unimplemented!(),
            Abort => unimplemented!(),
            Unreachable => Err(EvalError::Unreachable),
            Yield { .. } => unimplemented!(),
            GeneratorDrop => unimplemented!(),
            FalseEdges { .. } => unimplemented!(),
            FalseUnwind { .. } => unimplemented!(),
        }
    }

    /// Decides whether it is okay to call the method with signature `real_sig` using signature `sig`.
    /// FIXME: This should take into account the platform-dependent ABI description.
    fn check_sig_compat(
        &mut self,
        sig: ty::FnSig<'tcx>,
        real_sig: ty::FnSig<'tcx>,
    ) -> EvalResult<'tcx, bool> {
        fn check_ty_compat<'tcx>(
            ty: ty::Ty<'tcx>,
            real_ty: ty::Ty<'tcx>,
        ) -> bool {
            if ty == real_ty { return true; } // This is actually a fast pointer comparison
            return match (&ty.sty, &real_ty.sty) {
                // Permit changing the pointer type of raw pointers and references as well as
                // mutability of raw pointers.
                // TODO: Should not be allowed when fat pointers are involved.
                (&TypeVariants::TyRawPtr(_), &TypeVariants::TyRawPtr(_)) => true,
                (&TypeVariants::TyRef(_, _, _), &TypeVariants::TyRef(_, _, _)) =>
                    ty.is_mutable_pointer() == real_ty.is_mutable_pointer(),
                // rule out everything else
                _ => false
            }
        }

        if sig.abi == real_sig.abi &&
            sig.variadic == real_sig.variadic &&
            sig.inputs_and_output.len() == real_sig.inputs_and_output.len() &&
            sig.inputs_and_output.iter().zip(real_sig.inputs_and_output).all(|(ty, real_ty)| check_ty_compat(ty, real_ty)) {
            // Definitely good.
            return Ok(true);
        }

        if sig.variadic || real_sig.variadic {
            // We're not touching this
            return Ok(false);
        }

        // We need to allow what comes up when a non-capturing closure is cast to a fn().
        match (sig.abi, real_sig.abi) {
            (Abi::Rust, Abi::RustCall) // check the ABIs.  This makes the test here non-symmetric.
                if check_ty_compat(sig.output(), real_sig.output()) && real_sig.inputs_and_output.len() == 3 => {
                // First argument of real_sig must be a ZST
                let fst_ty = real_sig.inputs_and_output[0];
                if self.type_layout(fst_ty)?.is_zst() {
                    // Second argument must be a tuple matching the argument list of sig
                    let snd_ty = real_sig.inputs_and_output[1];
                    match snd_ty.sty {
                        TypeVariants::TyTuple(tys) if sig.inputs().len() == tys.len() =>
                            if sig.inputs().iter().zip(tys).all(|(ty, real_ty)| check_ty_compat(ty, real_ty)) {
                                return Ok(true)
                            },
                        _ => {}
                    }
                }
            }
            _ => {}

        };

        // Nope, this doesn't work.
        return Ok(false);
    }

    fn eval_fn_call(
        &mut self,
        instance: ty::Instance<'tcx>,
        destination: Option<(Place<'tcx>, mir::BasicBlock)>,
        arg_operands: &[mir::Operand<'tcx>],
        span: Span,
        sig: ty::FnSig<'tcx>,
    ) -> EvalResult<'tcx, Option<Vec<FinishStep<'tcx>>>> {
        trace!("eval_fn_call: {:#?}", instance);
        match instance.def {
            ty::InstanceDef::Intrinsic(..) => {
                let (ret, target) = match destination {
                    Some(dest) => dest,
                    _ => return Err(EvalError::Unreachable),
                };
                let ty = sig.output();
                if !is_inhabited(self.tcx, ty) {
                    return Err(EvalError::Unreachable);
                }
                let layout = self.type_layout(ty)?;
                self.call_intrinsic(instance, arg_operands, ret, ty, layout, target)?;
                self.dump_local(ret);
                Ok(None)
            },
            ty::InstanceDef::ClosureOnceShim{..} => {
                let mut args = Vec::new();
                for arg in arg_operands {
                    let arg_val = self.eval_operand(arg)?;
                    let arg_ty = self.operand_ty(arg);
                    args.push(ValTy { value: arg_val, ty: arg_ty});
                }
                if self.eval_fn_call_inner(
                    instance,
                    destination,
                    arg_operands,
                    span,
                    sig,
                )? {
                    return Ok(None);
                }
                let mut arg_locals = self.frame().mir.args_iter();
                match sig.abi {
                    // closure as closure once
                    Abi::RustCall => {
                        for (arg_local, arg_valty) in arg_locals.zip(args) {
                            let dest = self.eval_place(&mir::Place::Local(arg_local))?;
                            self.write_value(arg_valty, dest)?;
                        }
                    },
                    // non capture closure as fn ptr
                    // need to inject zst ptr for closure object (aka do nothing)
                    // and need to pack arguments
                    Abi::Rust => {
                        trace!("arg_locals: {:?}", self.frame().mir.args_iter().collect::<Vec<_>>());
                        trace!("arg_operands: {:?}", arg_operands);
                        let local = arg_locals.nth(1).unwrap();
                        for (i, arg_valty) in args.into_iter().enumerate() {
                            let dest = self.eval_place(&mir::Place::Local(local).field(mir::Field::new(i),
                                                                                        arg_valty.ty))?;
                            self.write_value(arg_valty, dest)?;
                        }
                    },
                    _ => bug!("bad ABI for ClosureOnceShim: {:?}", sig.abi),
                }
                Ok(None)
            }
            ty::InstanceDef::CloneShim(..) |
            ty::InstanceDef::FnPtrShim(..) |
            ty::InstanceDef::DropGlue(..) |
            ty::InstanceDef::Item(_) => {
                match sig.abi {
                    Abi::C => {
                        let ty = sig.output();
                        let (ret, target) = destination.unwrap();
                        return self.call_c_abi(instance.def_id(), arg_operands, ret, ty, target);
                    },
                    Abi::Rust | Abi::RustCall => {},
                    _ => unimplemented!(),
                }
                let mut args = Vec::new();
                for arg in arg_operands {
                    let arg_val = self.eval_operand(arg)?;
                    let arg_ty = self.operand_ty(arg);
                    args.push(ValTy { value: arg_val, ty: arg_ty });
                }

                if self.eval_fn_call_inner(
                    instance,
                    destination,
                    arg_operands,
                    span,
                    sig,
                )? {
                    return Ok(None);
                }

                let mut arg_locals = self.frame().mir.args_iter();
                trace!("ABI: {:?}", sig.abi);
                trace!("arg_locals: {:?}", self.frame().mir.args_iter().collect::<Vec<_>>());
                trace!("arg_operands: {:?}", arg_operands);
                match sig.abi {
                    Abi::Rust => {
                        for (arg_local, valty) in arg_locals.zip(args) {
                            let dest = self.eval_place(&mir::Place::Local(arg_local))?;
                            self.write_value(valty, dest)?;
                        }
                    }
                    Abi::RustCall => {
                        assert_eq!(args.len(), 2);

                        {   // write first argument
                            let first_local = arg_locals.next().unwrap();
                            let dest = self.eval_place(&mir::Place::Local(first_local))?;
                            self.write_value(ValTy { value: args[0].value, ty: args[0].ty }, dest)?;
                        }

                        // unpack and write all other args
                        let layout = self.type_layout(args[1].ty)?;
                        if let ty::TyTuple(..) = args[1].ty.sty {
                            if self.frame().mir.args_iter().count() == layout.fields.count() + 1 {
                                match args[1].value {
                                    Value::ByRef(ptr) => {
                                        for (i, arg_local) in arg_locals.enumerate() {
                                            let field = layout.field(&self, i)?;
                                            let offset = layout.fields.offset(i).bytes();
                                            let arg = Value::ByRef(ptr.offset(offset, (&self).data_layout())?);
                                            let dest =
                                                self.eval_place(&mir::Place::Local(arg_local))?;
                                            trace!(
                                                "writing arg {:?} to {:?} (type: {})",
                                                arg,
                                                dest,
                                                field.ty
                                            );
                                            self.write_value(ValTy { value: arg, ty: field.ty }, dest)?;
                                        }
                                    }
                                    Value::ByVal(PrimVal::Undef) => {}
                                    other => {
                                        // There can be at most one element in the tuple with nonzero size.
                                        let mut wrote_arg = false;
                                        for (i, arg_local) in arg_locals.enumerate() {
                                            let field = layout.field(&self, i)?;
                                            if layout.size == field.size {
                                                let dest =
                                                    self.eval_place(&mir::Place::Local(arg_local))?;
                                                self.write_value(ValTy { value: other, ty: field.ty }, dest)?;
                                                wrote_arg = true;
                                                break;
                                            }
                                        }
                                        if !wrote_arg {
                                            bug!("failed to unpack arguments from tuple {:?}", other)
                                        }
                                    }
                                }
                            } else {
                                trace!("manual impl of rust-call ABI");
                                // called a manual impl of a rust-call function
                                let dest = self.eval_place(
                                    &mir::Place::Local(arg_locals.next().unwrap()),
                                )?;
                                self.write_value(ValTy {value: args[1].value, ty: args[1].ty }, dest)?;
                            }
                        } else {
                            bug!(
                                "rust-call ABI tuple argument was {:#?}, {:#?}",
                                args[1].ty,
                                layout
                            );
                        }
                    }
                    _ => unimplemented!(),
                }
                Ok(None)
            },
            ty::InstanceDef::Virtual(_, idx) => {
                let ptr_size = self.memory.pointer_size();
                let (_, vtable) = self.eval_operand(&arg_operands[0])?.into_ptr_vtable_pair(&self.memory)?;
                let fn_ptr = self.memory.read_ptr(vtable.offset(ptr_size * (idx as u64 + 3), self.memory.layout)?)?;
                let instance = self.memory.get_fn(fn_ptr.to_ptr()?)?;
                let mut arg_operands = arg_operands.to_vec();
                let ty = self.operand_ty(&arg_operands[0]);
                let ty = self.get_field_ty(ty, 0)?.ty;
                match arg_operands[0] {
                    mir::Operand::Copy(ref mut lval) => *lval = lval.clone().field(mir::Field::new(0), ty),
                    mir::Operand::Move(ref mut lval) => *lval = lval.clone().field(mir::Field::new(0), ty),
                    _ => bug!("virtual call first arg cannot be a constant"),
                }
                // recurse with concrete function
                self.eval_fn_call(
                    instance,
                    destination,
                    &arg_operands,
                    span,
                    sig,
                )
            },
        }
    }

    /// Returns Ok(true) when the function was handled completely due to mir not being available
    fn eval_fn_call_inner(
        &mut self,
        instance: ty::Instance<'tcx>,
        destination: Option<(Place<'tcx>, mir::BasicBlock)>,
        arg_operands: &[mir::Operand<'tcx>],
        span: Span,
        sig: ty::FnSig<'tcx>,
    ) -> EvalResult<'tcx, bool> {
        trace!("eval_fn_call_inner: {:#?}, {:#?}", instance, destination);

        // Try to intercept some calls, regardless of whether MIR exists for them or not.
        // TODO: make this more robust than a string match.
        match instance.def {
            ty::InstanceDef::Item(def_id) => {
                match self.tcx.item_path_str(def_id).as_str() {
                    "std::io::stdin" => {
                        let (_lval, block) = destination.expect("std::io::stdin() does not diverge");
                        self.goto_block(block);
                        return Ok(true);
                    }
                    "<std::io::Stdin as std::io::Read>::read" => {
                        let (lval, block) = destination.expect("Stdin::read() does not diverge");
                        let args_res: EvalResult<Vec<Value>> = arg_operands.iter()
                            .map(|arg| self.eval_operand(arg))
                            .collect();
                        let args = args_res?;

                        let num_bytes = match args[1] {
                            Value::ByValPair(PrimVal::Ptr(ptr), PrimVal::Bytes(len)) => {
                                self.memory.write_fresh_abstract_bytes(ptr, len as u64)?;
                                len
                            }
                            _ => {
                                unimplemented!()
                            }
                        };

                        let _ = sig.output();

                        // Write `Ok(num_bytes)` to the return value.
                        let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                        let usize_bytes = self.memory.pointer_size();

                        // FIXME make this more robust
                        self.memory.write_uint(dest_ptr, 0, usize_bytes)?; // discriminant = Ok
                        self.memory.write_uint(
                            dest_ptr.offset(usize_bytes, self.memory.layout)?,
                            num_bytes, usize_bytes)?; // payload

                        self.goto_block(block);
                        return Ok(true);
                    }
                    "<std::io::Stdin as std::io::Read>::read_exact" => {
                        let (lval, block) = destination.expect("Stdin::read() does not diverge");
                        let args_res: EvalResult<Vec<Value>> = arg_operands.iter()
                            .map(|arg| self.eval_operand(arg))
                            .collect();
                        let args = args_res?;

                        match args[1] {
                            Value::ByValPair(PrimVal::Ptr(ptr), PrimVal::Bytes(len)) => {
                                self.memory.write_fresh_abstract_bytes(ptr, len as u64)?;
                            }
                            _ => {
                                unimplemented!()
                            }
                        }

                        let dest_ty = sig.output();

                        // FIXME make this more robust
                        self.write_discriminant_value(
                            dest_ty,
                            lval,
                            0)?;

                        self.goto_block(block);
                        return Ok(true);
                    }

                    "std::io::Stdin::lock" => {
                        return Err(
                            EvalError::Unimplemented(
                                "no abstract implementation for stdin.lock()".into()));
                    }
                    _ => (),
                }
            }
            _ => (),
        }

        // Only trait methods can have a Self parameter.

        let mir = match self.load_mir(instance.def) {
            Ok(mir) => mir,
            Err(EvalError::NoMirFor(path)) => {
                self.call_missing_fn(instance, destination, arg_operands, sig, path)?;
                return Ok(true);
            }
            Err(other) => return Err(other),
        };

        let (return_place, return_to_block) = match destination {
            Some((place, block)) => (place, StackPopCleanup::Goto(block)),
            None => (Place::undef(), StackPopCleanup::None),
        };

        self.push_stack_frame(
            instance,
            span,
            mir,
            return_place,
            return_to_block,
        )?;

        Ok(false)
    }

    /// Returns Ok() when the function was handled, fail otherwise
    fn call_missing_fn(
        &mut self,
        instance: ty::Instance<'tcx>,
        destination: Option<(Place<'tcx>, mir::BasicBlock)>,
        arg_operands: &[mir::Operand<'tcx>],
        sig: ty::FnSig<'tcx>,
        path: String,
    ) -> EvalResult<'tcx> {
        if sig.abi == Abi::C {
            // An external C function
            let ty = sig.output();
            let (ret, target) = destination.unwrap();
            self.call_c_abi(instance.def_id(), arg_operands, ret, ty, target)?;
            return Ok(());
        }

        // A Rust function is missing, which means we are running with MIR missing for libstd (or other dependencies).
        // Still, we can make many things mostly work by "emulating" or ignoring some functions.

        let args_res: EvalResult<Vec<Value>> = arg_operands.iter()
            .map(|arg| self.eval_operand(arg))
            .collect();
        let args = args_res?;

        match &path[..] {
            "std::io::_print" => {
                trace!("Ignoring output.  To run programs that print, make sure you have a libstd with full MIR.");
                self.goto_block(destination.unwrap().1);
                Ok(())
            },
            "std::thread::Builder::new" => Err(EvalError::Unimplemented("miri does not support threading".to_owned())),
            "std::env::args" => Err(EvalError::Unimplemented("miri does not support program arguments".to_owned())),
            "std::panicking::rust_panic_with_hook" |
            "std::rt::begin_panic_fmt" => Err(EvalError::Panic),
            "std::panicking::panicking" |
            "std::rt::panicking" => {
                let (lval, block) = destination.expect("std::rt::panicking does not diverge");
                // we abort on panic -> `std::rt::panicking` always returns false
                let bool = self.tcx.types.bool;
                self.write_primval(lval, PrimVal::from_bool(false), bool)?;
                self.goto_block(block);
                Ok(())
            }
            "alloc::allocator::Layout::from_size_align" => {
                let (lval, block) = destination.expect("from_size_align() does not diverge");
                let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                let usize = self.tcx.types.usize;
                let size = self.value_to_primval(args[0], usize)?.to_u128()?;
                let align = self.value_to_primval(args[1], usize)?.to_u128()?;

                if !align.is_power_of_two() {
                    unimplemented!();
                }

                if size as usize > ::std::usize::MAX - (align as usize - 1) {
                    unimplemented!();
                }

                let usize_bytes = self.memory.pointer_size();

                // FIXME make this more robust
                self.memory.write_uint(dest_ptr, 1, usize_bytes)?; // discriminant = Some

                // payload
                self.memory.write_uint(dest_ptr.offset(usize_bytes, self.memory.layout)?,
                                       size, usize_bytes)?;
                self.memory.write_uint(dest_ptr.offset(usize_bytes * 2, self.memory.layout)?,
                                       align, usize_bytes)?;

                self.goto_block(block);
                return Ok(());
            }

            "alloc::alloc::::__rust_alloc" => {
                let usize = self.tcx.types.usize;
                let size = self.value_to_primval(args[0], usize)?.to_u64()?;
                let align = self.value_to_primval(args[1], usize)?.to_u64()?;
                let ptr = self.memory.allocate(size, align)?;

                let (lval, block) = destination.expect("alloc() does not diverge");
                let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                self.memory.write_ptr(dest_ptr, ptr)?;
                self.goto_block(block);
                return Ok(());
            }

            "alloc::alloc::::__rust_alloc_zeroed" => {
                let usize = self.tcx.types.usize;
                let size = self.value_to_primval(args[0], usize)?.to_u64()?;
                let align = self.value_to_primval(args[1], usize)?.to_u64()?;
                let ptr = self.memory.allocate(size, align)?;
                self.memory.write_repeat(ptr, 0, size)?;


                let (lval, block) = destination.expect("alloc() does not diverge");
                let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                self.memory.write_ptr(dest_ptr, ptr)?;
                self.goto_block(block);
                return Ok(());
            }

            "alloc::allocator::Layout::size" => {
                let (lval, block) = destination.expect("size() does not diverge");

                let usize_bytes = self.memory.pointer_size();
                let self_size = match args[0] {
                    Value::ByVal(PrimVal::Ptr(ptr)) => {
                        self.memory.read_uint(ptr, usize_bytes)?
                    }
                    _ => unreachable!(),
                };

                self.write_primval(lval, PrimVal::Bytes(self_size), sig.output())?;
                self.goto_block(block);
                return Ok(());
            }

            "alloc::allocator::Layout::repeat" => {
                let (lval, block) = destination.expect("repeat() does not diverge");
                let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                let usize_bytes = self.memory.pointer_size();
                let (self_size, self_align) = match args[0] {
                    Value::ByVal(PrimVal::Ptr(ptr)) => {
                        (self.memory.read_uint(ptr, usize_bytes)? as u64,
                         self.memory.read_uint(ptr.offset(usize_bytes, self.memory.layout)?,
                                               usize_bytes)? as u64)
                    }
                    _ => unreachable!(),
                };

                let usize = self.tcx.types.usize;
                let n = self.value_to_primval(args[1], usize)?.to_u64()?;

                let padding_needed = {
                    let len = self_size;
                    let len_rounded_up =
                        len.wrapping_add(self_align).wrapping_sub(1) & !self_align.wrapping_sub(1);
                    len_rounded_up.wrapping_sub(len)
                };

                let padded_size = match self_size.checked_add(padding_needed) {
                    None => unimplemented!(), // return None
                    Some(padded_size) => padded_size,
                };
                let alloc_size = match padded_size.checked_mul(n) {
                    None => unimplemented!(), // return None
                    Some(alloc_size) => alloc_size,
                };

                let usize_bytes = self.memory.pointer_size();

                self.memory.write_uint(dest_ptr, 1, usize_bytes)?; // discriminant = Some

                // payload
                self.memory.write_uint(dest_ptr.offset(usize_bytes, self.memory.layout)?,
                                       alloc_size as u128, usize_bytes)?;
                self.memory.write_uint(dest_ptr.offset(usize_bytes * 2, self.memory.layout)?,
                                       self_align as u128, usize_bytes)?;
                self.memory.write_uint(
                    dest_ptr.offset(usize_bytes * 3, self.memory.layout)?,
                    padded_size as u128, usize_bytes)?;

                self.goto_block(block);
                return Ok(());
            }

            "alloc::alloc::::__rust_realloc" => {
                let (lval, block) = destination.expect("realloc() does not diverge");
                let dest_ptr = self.force_allocation(lval)?.to_ptr()?;

                let ptr = match args[0] {
                    Value::ByVal(PrimVal::Ptr(p)) => p,
                    _ => unimplemented!(),
                };

                let usize = self.tcx.types.usize;
                let _old_size = self.value_to_primval(args[1], usize)?.to_u64()?;
                let align = self.value_to_primval(args[2], usize)?.to_u64()?;
                let new_size = self.value_to_primval(args[3], usize)?.to_u64()?;

                let new_ptr = self.memory.reallocate(ptr, new_size, align)?;
                self.memory.write_ptr(dest_ptr, new_ptr)?;
                self.goto_block(block);
                return Ok(());
            }

            "alloc::alloc::::__rust_dealloc" => {
                let (_lval, block) = destination.expect("dealloc() does not diverge");

                let ptr = match args[0] {
                    Value::ByVal(PrimVal::Ptr(p)) => p,
                    _ => unimplemented!(),
                };

                self.memory.deallocate(ptr)?;
                self.goto_block(block);
                return Ok(());
            }

            _ => Err(EvalError::NoMirFor(path)),
        }
    }

    fn call_c_abi(
        &mut self,
        def_id: DefId,
        args: &[mir::Operand<'tcx>],
        dest: Place<'tcx>,
        dest_ty: Ty<'tcx>,
        target: mir::BasicBlock,
    ) -> EvalResult<'tcx, Option<Vec<FinishStep<'tcx>>>> {
        let attrs = self.tcx.get_attrs(def_id);
        let link_name = match attr::first_attr_value_str_by_name(&attrs, "link_name") {
            Some(name) => name.as_str(),
            None => self.tcx.item_name(def_id).as_str(),
        };

        let args_res: EvalResult<Vec<Value>> = args.iter()
            .map(|arg| self.eval_operand(arg))
            .collect();
        let args = args_res?;

        let usize = self.tcx.types.usize;

        match &link_name[..] {
            "memcmp" => {
                let left = args[0].read_ptr(&self.memory)?;
                let right = args[1].read_ptr(&self.memory)?;
                let n = self.value_to_primval(args[2], usize)?.to_u64()?;

                let mut is_concrete = true;
                let mut abstract_branches = Vec::new();
                let mut equal_constraints = Vec::new();

                let result = {
                    use std::cmp::Ordering::*;

                    let left_bytes = self.memory.read_bytes(left, n)?;
                    let right_bytes = self.memory.read_bytes(right, n)?;

                    let mut ordering = Equal;
                    'stepping: for idx in 0..n as usize {
                        let (left, right) = match (left_bytes[idx], right_bytes[idx]) {
                            (SByte::Concrete(c0), SByte::Concrete(c1)) => {
                                if c0 == c1 {
                                    continue 'stepping;
                                } else {
                                    if c0 < c1 {
                                        ordering = Less;
                                    } else {
                                        ordering = Greater;
                                    }
                                    break 'stepping;
                                }
                            }
                            (SByte::Abstract(a), SByte::Concrete(c)) => {
                                is_concrete = false;
                                let mut sbytes = [SByte::Concrete(0); 8];
                                sbytes[0] = SByte::Abstract(a);
                                (PrimVal::Abstract(sbytes), PrimVal::from_u128(c as u128))
                            }
                            (SByte::Concrete(c), SByte::Abstract(a)) => {
                                is_concrete = false;
                                let mut sbytes = [SByte::Concrete(0); 8];
                                sbytes[0] = SByte::Abstract(a);
                                (PrimVal::from_u128(c as u128), PrimVal::Abstract(sbytes))
                            }
                            (SByte::Abstract(aleft), SByte::Abstract(aright)) => {
                                is_concrete = false;
                                let mut sbytes_left = [SByte::Concrete(0); 8];
                                sbytes_left[0] = SByte::Abstract(aleft);
                                let mut sbytes_right = [SByte::Concrete(0); 8];
                                sbytes_right[0] = SByte::Abstract(aright);
                                (PrimVal::Abstract(sbytes_left), PrimVal::Abstract(sbytes_right))
                            }
                        };

                        let mut lt_constraints = equal_constraints.clone();
                        let mut gt_constraints = equal_constraints.clone();

                        equal_constraints.push(
                            Constraint::new_compare(
                                mir::BinOp::Eq, PrimValKind::U8,
                                left, right));

                        lt_constraints.push(
                            Constraint::new_compare(
                                mir::BinOp::Lt, PrimValKind::U8,
                                left, right));

                        gt_constraints.push(
                            Constraint::new_compare(
                                mir::BinOp::Gt, PrimValKind::U8,
                                left, right));

                        if self.memory.constraints.is_feasible_with(&lt_constraints) {
                            abstract_branches.push(
                                FinishStep {
                                    constraints: lt_constraints,
                                    variant: FinishStepVariant::Continue {
                                        goto_block: target,
                                        set_place: Some(
                                            (dest, PrimVal::from_i128(-1), dest_ty)),
                                    },
                                });
                        }

                        if self.memory.constraints.is_feasible_with(&gt_constraints) {
                            abstract_branches.push(
                                FinishStep {
                                    constraints: gt_constraints,
                                    variant: FinishStepVariant::Continue {
                                        goto_block: target,
                                        set_place: Some(
                                            (dest, PrimVal::from_u128(1), dest_ty)),
                                    },
                                });
                        }
                    }

                    match ordering {
                        Less => -1i8,
                        Equal => 0,
                        Greater => 1,
                    }
                };

                if is_concrete {
                    self.write_primval(dest, PrimVal::Bytes(result as u128), dest_ty)?;
                    self.goto_block(target);
                } else {
                    if self.memory.constraints.is_feasible_with(&equal_constraints) {
                        abstract_branches.push(FinishStep {
                            constraints: equal_constraints,
                            variant: FinishStepVariant::Continue {
                                goto_block: target,
                                set_place: Some((dest, PrimVal::from_u128(0), dest_ty)),
                            },
                        });
                    }
                    return Ok(Some(abstract_branches));
                }
            }

            "memrchr" => {
                unimplemented!()
                    /*
                let ptr = args[0].read_ptr(&self.memory)?;
                let val = self.value_to_primval(args[1], usize)?.to_u64()? as u8;
                let num = self.value_to_primval(args[2], usize)?.to_u64()?;
                if let Some(idx) = self.memory.read_bytes(ptr, num)?.iter().rev().position(|&c| c == val) {
                    let new_ptr = ptr.offset(num - idx as u64 - 1);
                    self.write_value(Value::ByVal(PrimVal::Ptr(new_ptr)), dest, dest_ty)?;
                } else {
                    self.write_value(Value::ByVal(PrimVal::Bytes(0)), dest, dest_ty)?;
                }*/
            }

            "memchr" => {
                unimplemented!()
                /*
                let ptr = args[0].read_ptr(&self.memory)?;
                let val = self.value_to_primval(args[1], usize)?.to_u64()? as u8;
                let num = self.value_to_primval(args[2], usize)?.to_u64()?;
                if let Some(idx) = self.memory.read_bytes(ptr, num)?.iter().position(|&c| c == val) {
                    let new_ptr = ptr.offset(idx as u64);
                    self.write_value(Value::ByVal(PrimVal::Ptr(new_ptr)), dest, dest_ty)?;
                } else {
                    self.write_value(Value::ByVal(PrimVal::Bytes(0)), dest, dest_ty)?;
                }*/
            }

            "getenv" => {
                let result = {
                    let name_ptr = args[0].read_ptr(&self.memory)?.to_ptr()?;
                    let name = self.memory.read_c_str(name_ptr)?;
                    match self.env_vars.get(name) {
                        Some(&var) => PrimVal::Ptr(var),
                        None => PrimVal::Bytes(0),
                    }
                };
                self.write_primval(dest, result, dest_ty)?;
                self.goto_block(target);
            }

            // unix panic code inside libstd will read the return value of this function
            "pthread_rwlock_rdlock" => {
                self.write_primval(dest, PrimVal::Bytes(0), dest_ty)?;
                self.goto_block(target);
            }

            link_name if link_name.starts_with("pthread_") => {
                warn!("ignoring C ABI call: {}", link_name);
                self.goto_block(target);
                return Ok(None);
            },

            _ => {
                return Err(EvalError::Unimplemented(format!("can't call C ABI function: {}", link_name)));
            }
        }

        // Since we pushed no stack frame, the main loop will act
        // as if the call just completed and it's returning to the
        // current frame.
        Ok(None)
    }
}
