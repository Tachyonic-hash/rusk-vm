// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use canonical::{Canon, CanonError, Sink, Source};
use dusk_abi::{ContractState, Query, ReturnValue, Transaction};

use wasmi::{
    Externals, ImportsBuilder, MemoryRef, ModuleImportResolver, RuntimeArgs,
    RuntimeValue, Trap, TrapKind,
};

use crate::contract::ContractId;
use crate::gas::GasMeter;
use crate::state::NetworkState;
use crate::VMError;

pub trait Resolver: Invoke + ModuleImportResolver + Clone + Default {}

pub use crate::resolver::CompoundResolver as StandardABI;

#[derive(Debug)]
enum Argument {
    Query(Query),
    Transaction(Transaction),
}

pub struct StackFrame {
    callee: ContractId,
    argument: Argument,
    ret: ReturnValue,
    memory: MemoryRef,
}

impl std::fmt::Debug for StackFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(arg: {:?} return: {:?})", self.argument, self.ret)
    }
}

impl StackFrame {
    fn new_query(callee: ContractId, memory: MemoryRef, query: Query) -> Self {
        StackFrame {
            callee,
            memory,
            argument: Argument::Query(query),
            ret: Default::default(),
        }
    }

    fn new_transaction(
        callee: ContractId,
        memory: MemoryRef,
        transaction: Transaction,
    ) -> Self {
        StackFrame {
            callee,
            memory,
            argument: Argument::Transaction(transaction),
            ret: Default::default(),
        }
    }

    fn memory<R, C: FnOnce(&[u8]) -> R>(&self, closure: C) -> R {
        self.memory.with_direct_access(closure)
    }

    fn memory_mut<R, C: FnOnce(&mut [u8]) -> R>(&self, closure: C) -> R {
        self.memory.with_direct_access_mut(closure)
    }
}

pub trait Invoke: Sized {
    fn invoke(
        context: &mut CallContext,
        index: usize,
        args: RuntimeArgs,
    ) -> Result<Option<RuntimeValue>, VMError>;
}

pub struct CallContext<'a> {
    state: &'a mut NetworkState,
    stack: Vec<StackFrame>,
    gas_meter: &'a mut GasMeter,
}

impl<'a> CallContext<'a> {
    pub fn new(
        state: &'a mut NetworkState,
        gas_meter: &'a mut GasMeter,
    ) -> Self {
        CallContext {
            state,
            stack: vec![],
            gas_meter,
        }
    }

    pub fn query(
        &mut self,
        target: ContractId,
        query: Query,
    ) -> Result<ReturnValue, VMError> {
        let resolver = StandardABI::default();
        let imports = ImportsBuilder::new()
            .with_resolver("env", &resolver)
            .with_resolver("canon", &resolver);

        let instance;

        if let Some(module) = self.state.modules().borrow().get(&target) {
            // is this a reserved module call?
            return module.execute(query).map_err(VMError::from_store_error);
        } else {
            let contract = self.state.get_contract(&target)?;

            let module = wasmi::Module::from_buffer(contract.bytecode())?;

            instance = wasmi::ModuleInstance::new(&module, &imports)?
                .assert_no_start();

            match instance.export_by_name("memory") {
                Some(wasmi::ExternVal::Memory(memref)) => {
                    // write contract state and argument to memory
                    memref
                        .with_direct_access_mut(|m| {
                            let mut sink = Sink::new(&mut *m);
                            // copy the raw bytes only, since the
                            // contract
                            // can infer
                            // it's own state and argument lengths
                            sink.copy_bytes(contract.state().as_bytes());
                            sink.copy_bytes(query.as_bytes());
                            Ok(())
                        })
                        .map_err(VMError::from_store_error)?;

                    self.stack
                        .push(StackFrame::new_query(target, memref, query));
                }
                _ => return Err(VMError::MemoryNotFound),
            }
        }

        // Perform the query call
        instance.invoke_export("q", &[wasmi::RuntimeValue::I32(0)], self)?;

        match instance.export_by_name("memory") {
            Some(wasmi::ExternVal::Memory(memref)) => memref
                .with_direct_access_mut(|m| {
                    let mut source = Source::new(&m[..]);
                    let result = ReturnValue::decode(&mut source)?;

                    self.stack.pop();
                    Ok(result)
                })
                .map_err(VMError::from_store_error),
            _ => Err(VMError::MemoryNotFound),
        }
    }

    pub fn transact(
        &mut self,
        target: ContractId,
        transaction: Transaction,
    ) -> Result<(ContractState, ReturnValue), VMError> {
        let resolver = StandardABI::default();
        let imports = ImportsBuilder::new()
            .with_resolver("env", &resolver)
            .with_resolver("canon", &resolver);

        let instance;

        {
            let contract = self.state.get_contract(&target)?;
            let module = wasmi::Module::from_buffer(contract.bytecode())?;

            instance = wasmi::ModuleInstance::new(&module, &imports)?
                .assert_no_start();

            match instance.export_by_name("memory") {
                Some(wasmi::ExternVal::Memory(memref)) => {
                    // write contract state and argument to memory

                    memref.with_direct_access_mut(|m| {
                        let mut sink = Sink::new(&mut *m);
                        // copy the raw bytes only, since the contract can
                        // infer it's own state and argument lengths.
                        sink.copy_bytes(contract.state().as_bytes());
                        sink.copy_bytes(transaction.as_bytes());
                    });

                    self.stack.push(StackFrame::new_transaction(
                        target,
                        memref,
                        transaction,
                    ));
                }
                _ => return Err(VMError::MemoryNotFound),
            }
        }
        // Perform the transact call
        instance.invoke_export("t", &[wasmi::RuntimeValue::I32(0)], self)?;

        let ret = {
            let mut contract = self.state.get_contract_mut(&target)?;

            match instance.export_by_name("memory") {
                Some(wasmi::ExternVal::Memory(memref)) => {
                    memref
                        .with_direct_access_mut(|m| {
                            let mut source = Source::new(&m[..]);

                            // read new state
                            let state = ContractState::decode(&mut source)?;

                            // update new self state
                            *(*contract).state_mut() = state;

                            // read return value
                            dbg!(ReturnValue::decode(&mut source))
                        })
                        .map_err(VMError::from_store_error)
                }
                _ => return Err(VMError::MemoryNotFound),
            }
        };

        let state = if self.stack.len() > 1 {
            self.stack.pop();
            self.state.get_contract(self.callee())?.state().clone()
        } else {
            let state = self.state.get_contract(self.callee())?.state().clone();
            self.stack.pop();
            state
        };

        Ok((state, ret?))
    }

    pub fn gas_meter_mut(&mut self) -> &mut GasMeter {
        self.gas_meter
    }

    pub fn top(&self) -> &StackFrame {
        self.stack.last().expect("Invalid stack")
    }

    pub fn callee(&self) -> &ContractId {
        &self.top().callee
    }

    pub fn memory<R, C: FnOnce(&[u8]) -> R>(&self, closure: C) -> R {
        self.top().memory(closure)
    }

    pub fn memory_mut<R, C: FnOnce(&mut [u8]) -> Result<R, CanonError>>(
        &mut self,
        closure: C,
    ) -> Result<R, CanonError> {
        self.stack
            .last_mut()
            .expect("Invalid stack")
            .memory_mut(closure)
    }

    pub fn state(&self) -> &NetworkState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut NetworkState {
        &mut self.state
    }
}

/// Convenience function to construct host traps
pub fn host_trap(host: VMError) -> Trap {
    Trap::new(TrapKind::Host(Box::new(host)))
}

impl<'a> Externals for CallContext<'a> {
    fn invoke_index(
        &mut self,
        index: usize,
        args: RuntimeArgs,
    ) -> Result<Option<RuntimeValue>, Trap> {
        match StandardABI::invoke(self, index, args) {
            Ok(ok) => Ok(ok),
            Err(e) => {
                if let VMError::Trap(t) = e {
                    Err(t)
                } else {
                    Err(host_trap(e))
                }
            }
        }
    }
}
