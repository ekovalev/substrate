// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! This module defines `HostState` and `HostContext` structs which provide logic and state
//! required for execution of host.

use log::trace;
use wasmtime::{Caller, Func, Val};

use codec::{Decode, Encode};
use sc_allocator::{AllocationStats, FreeingBumpHeapAllocator};
use sc_executor_common::{
	error::Result,
	sandbox::{self, SupervisorFuncIndex, SandboxInstance},
	util::MemoryTransfer,
};
use sp_sandbox::env as sandbox_env;
use sp_wasm_interface::{FunctionContext, MemoryId, Pointer, Sandbox, WordSize, HostPointer};

use crate::{instance_wrapper::MemoryWrapper, runtime::StoreData, util};

// The sandbox store is inside of a Option<Box<..>>> so that we can temporarily borrow it.
struct SandboxStore(Option<Box<sandbox::Store<Func>>>);

// There are a bunch of `Rc`s within the sandbox store, however we only manipulate
// those within one thread so this should be safe.
unsafe impl Send for SandboxStore {}

/// The state required to construct a HostContext context. The context only lasts for one host
/// call, whereas the state is maintained for the duration of a Wasm runtime call, which may make
/// many different host calls that must share state.
pub struct HostState {
	sandbox_store: SandboxStore,
	/// The allocator instance to keep track of allocated memory.
	///
	/// This is stored as an `Option` as we need to temporarly set this to `None` when we are
	/// allocating/deallocating memory. The problem being that we can only mutable access `caller`
	/// once.
	allocator: Option<FreeingBumpHeapAllocator>,
	panic_message: Option<String>,
}

impl HostState {
	/// Constructs a new `HostState`.
	pub fn new(allocator: FreeingBumpHeapAllocator) -> Self {
		HostState {
			sandbox_store: SandboxStore(Some(Box::new(sandbox::Store::new(
				sandbox::SandboxBackend::TryWasmer,
			)))),
			allocator: Some(allocator),
			panic_message: None,
		}
	}

	/// Takes the error message out of the host state, leaving a `None` in its place.
	pub fn take_panic_message(&mut self) -> Option<String> {
		self.panic_message.take()
	}

	pub(crate) fn allocation_stats(&self) -> AllocationStats {
		self.allocator.as_ref()
			.expect("Allocator is always set and only unavailable when doing an allocation/deallocation; qed")
			.stats()
	}
}

/// A `HostContext` implements `FunctionContext` for making host calls from a Wasmtime
/// runtime. The `HostContext` exists only for the lifetime of the call and borrows state from
/// a longer-living `HostState`.
pub(crate) struct HostContext<'a> {
	pub(crate) caller: Caller<'a, StoreData>,
}

impl<'a> HostContext<'a> {
	fn host_state(&self) -> &HostState {
		self.caller
			.data()
			.host_state()
			.expect("host state is not empty when calling a function in wasm; qed")
	}

	fn host_state_mut(&mut self) -> &mut HostState {
		self.caller
			.data_mut()
			.host_state_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
	}

	fn sandbox_store(&self) -> &sandbox::Store<Func> {
		self.host_state()
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
	}

	fn sandbox_store_mut(&mut self) -> &mut sandbox::Store<Func> {
		self.host_state_mut()
			.sandbox_store
			.0
			.as_mut()
			.expect("sandbox store is only empty when temporarily borrowed")
	}
}

impl<'a> sp_wasm_interface::FunctionContext for HostContext<'a> {
	fn read_memory_into(
		&self,
		address: Pointer<u8>,
		dest: &mut [u8],
	) -> sp_wasm_interface::Result<()> {
		util::read_memory_into(&self.caller, address, dest).map_err(|e| e.to_string())
	}

	fn write_memory(&mut self, address: Pointer<u8>, data: &[u8]) -> sp_wasm_interface::Result<()> {
		util::write_memory_from(&mut self.caller, address, data).map_err(|e| e.to_string())
	}

	fn allocate_memory(&mut self, size: WordSize) -> sp_wasm_interface::Result<Pointer<u8>> {
		let memory = self.caller.data().memory();
		let mut allocator = self
			.host_state_mut()
			.allocator
			.take()
			.expect("allocator is not empty when calling a function in wasm; qed");

		// We can not return on error early, as we need to store back allocator.
		let res = allocator
			.allocate(&mut MemoryWrapper(&memory, &mut self.caller), size)
			.map_err(|e| e.to_string());

		self.host_state_mut().allocator = Some(allocator);

		res
	}

	fn deallocate_memory(&mut self, ptr: Pointer<u8>) -> sp_wasm_interface::Result<()> {
		let memory = self.caller.data().memory();
		let mut allocator = self
			.host_state_mut()
			.allocator
			.take()
			.expect("allocator is not empty when calling a function in wasm; qed");

		// We can not return on error early, as we need to store back allocator.
		let res = allocator
			.deallocate(&mut MemoryWrapper(&memory, &mut self.caller), ptr)
			.map_err(|e| e.to_string());

		self.host_state_mut().allocator = Some(allocator);

		res
	}

	fn sandbox(&mut self) -> &mut dyn Sandbox {
		self
	}

	fn register_panic_error_message(&mut self, message: &str) {
		self.host_state_mut().panic_message = Some(message.to_owned());
	}
}

impl<'a> Sandbox for HostContext<'a> {
	fn memory_get(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		buf_ptr: Pointer<u8>,
		buf_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self.sandbox_store().memory(memory_id).map_err(|e| e.to_string())?;

		let len = buf_len as usize;

		let buffer = match sandboxed_memory.read(Pointer::new(offset as u32), len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if util::write_memory_from(&mut self.caller, buf_ptr, &buffer).is_err() {
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS)
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_set(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		val_ptr: Pointer<u8>,
		val_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self.sandbox_store().memory(memory_id).map_err(|e| e.to_string())?;

		let len = val_len as usize;

		let buffer = match util::read_memory(&self.caller, val_ptr, len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if sandboxed_memory.write_from(Pointer::new(offset as u32), &buffer).is_err() {
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS)
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_teardown(&mut self, memory_id: MemoryId) -> sp_wasm_interface::Result<()> {
		self.sandbox_store_mut().memory_teardown(memory_id).map_err(|e| e.to_string())
	}

	fn memory_new(&mut self, initial: u32, maximum: u32) -> sp_wasm_interface::Result<u32> {
		self.sandbox_store_mut().new_memory(initial, maximum).map_err(|e| e.to_string())
	}

	fn invoke(
		&mut self,
		instance_id: u32,
		export_name: &str,
		mut args: &[u8],
		return_val: Pointer<u8>,
		return_val_len: u32,
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		trace!(target: "sp-sandbox", "invoke, instance_idx={}", instance_id);

		// Deserialize arguments and convert them into wasmi types.
		let args = Vec::<sp_wasm_interface::Value>::decode(&mut args)
			.map_err(|_| "Can't decode serialized arguments for the invocation")?
			.into_iter()
			.collect::<Vec<_>>();

		let instance = self.sandbox_store().instance(instance_id).map_err(|e| e.to_string())?;

		let dispatch_thunk =
			self.sandbox_store().dispatch_thunk(instance_id).map_err(|e| e.to_string())?;

		let result = instance.invoke(
			export_name,
			&args,
			&mut SandboxContext { host_context: self, dispatch_thunk, state },
		);

		match result {
			Ok(None) => Ok(sandbox_env::ERR_OK),
			Ok(Some(val)) => {
				// Serialize return value and write it back into the memory.
				sp_wasm_interface::ReturnValue::Value(val.into()).using_encoded(|val| {
					if val.len() > return_val_len as usize {
						return Err("Return value buffer is too small".into())
					}
					<HostContext as FunctionContext>::write_memory(self, return_val, val)
						.map_err(|_| "can't write return value")?;
					Ok(sandbox_env::ERR_OK)
				})
			},
			Err(_) => Ok(sandbox_env::ERR_EXECUTION),
		}
	}

	fn instance_teardown(&mut self, instance_id: u32) -> sp_wasm_interface::Result<()> {
		self.sandbox_store_mut()
			.instance_teardown(instance_id)
			.map_err(|e| e.to_string())
	}

	fn instance_new(
		&mut self,
		dispatch_thunk_id: u32,
		wasm: &[u8],
		raw_env_def: &[u8],
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		// Extract a dispatch thunk from the instance's table by the specified index.
		let dispatch_thunk = {
			let table = self
				.caller
				.data()
				.table()
				.ok_or("Runtime doesn't have a table; sandbox is unavailable")?;
			let table_item = table.get(&mut self.caller, dispatch_thunk_id);

			*table_item
				.ok_or("dispatch_thunk_id is out of bounds")?
				.funcref()
				.ok_or("dispatch_thunk_idx should be a funcref")?
				.ok_or("dispatch_thunk_idx should point to actual func")?
		};

		let guest_env = match sandbox::GuestEnvironment::decode(self.sandbox_store(), raw_env_def) {
			Ok(guest_env) => guest_env,
			Err(_) => return Ok(sandbox_env::ERR_MODULE as u32),
		};

		let mut store = self
			.host_state_mut()
			.sandbox_store
			.0
			.take()
			.expect("sandbox store is only empty when borrowed");

		// Catch any potential panics so that we can properly restore the sandbox store
		// which we've destructively borrowed.
		let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			store.instantiate(
				wasm,
				guest_env,
				&mut SandboxContext {
					host_context: self,
					dispatch_thunk: dispatch_thunk.clone(),
					state,
				},
			)
		}));

		self.host_state_mut().sandbox_store.0 = Some(store);

		let result = match result {
			Ok(result) => result,
			Err(error) => std::panic::resume_unwind(error),
		};

		let instance_idx_or_err_code = match result {
			Ok(instance) => instance.register(self.sandbox_store_mut(), dispatch_thunk),
			Err(sandbox::InstantiationError::StartTrapped) => sandbox_env::ERR_EXECUTION,
			Err(_) => sandbox_env::ERR_MODULE,
		};

		Ok(instance_idx_or_err_code as u32)
	}

	fn get_global_val(
		&self,
		instance_idx: u32,
		name: &str,
	) -> sp_wasm_interface::Result<Option<sp_wasm_interface::Value>> {
		self.sandbox_store()
			.instance(instance_idx)
			.map(|i| i.get_global_val(name))
			.map_err(|e| e.to_string())
	}

	fn get_global_i64(
		&self,
		instance_idx: u32,
		name: &str,
	) -> sp_wasm_interface::Result<Option<i64>> {
		self.sandbox_store()
			.instance(instance_idx)
			.map(|i| i.get_global_i64(name))
			.map_err(|e| e.to_string())
	}

	fn set_global_i64(
		&self,
		instance_idx: u32,
		name: &str,
		value: i64,
	) -> sp_wasm_interface::Result<u32> {
		trace!(target: "sp-sandbox", "set_global_i64, instance_idx={}", instance_idx);

		let instance = self.sandbox_store().instance(instance_idx).map_err(|e| e.to_string())?;

		let result = instance.set_global_i64(name, value);

		trace!(target: "sp-sandbox", "set_global_i64, name={name}, value={value:?}, result={result:?}");
		match result {
			Ok(None) => Ok(sandbox_env::ERROR_GLOBALS_NOT_FOUND),
			Ok(Some(_)) => Ok(sandbox_env::ERROR_GLOBALS_OK),
			Err(_) => Ok(sandbox_env::ERROR_GLOBALS_OTHER),
		}
	}

	fn memory_size(&mut self, memory_id: MemoryId) -> sp_wasm_interface::Result<u32> {
		let mut m = self
			.sandbox_store()
			.memory(memory_id)
			.map_err(|e| format!("Cannot get wasmer memory: {}", e))?;
		Ok(m.memory_size())
	}

	fn memory_grow(&mut self, memory_id: MemoryId, pages_num: u32) -> sp_wasm_interface::Result<u32> {
		let mut m = self.sandbox_store().memory(memory_id)
			.map_err(|e| format!("Cannot get wasmer memory: {}", e))?;
		m.memory_grow(pages_num).map_err(|e| format!("{}", e))
	}

	fn get_buff(&mut self, memory_id: MemoryId) -> sp_wasm_interface::Result<HostPointer> {
		let mut m = self.sandbox_store().memory(memory_id)
			.map_err(|e| format!("Cannot get wasmer memory: {}", e))?;
		Ok(m.get_buff() as HostPointer)
	}

	fn get_instance_ptr(&mut self, instance_id: u32) -> sp_wasm_interface::Result<HostPointer> {
		let instance = self.sandbox_store()
			.instance(instance_id)
			.map_err(|e| e.to_string())?;

		Ok(instance.as_ref().get_ref() as *const SandboxInstance as HostPointer)
	}
}

struct SandboxContext<'a, 'b> {
	host_context: &'a mut HostContext<'b>,
	dispatch_thunk: Func,
	/// Custom data to propagate it in supervisor export functions
	state: u32,
}

impl<'a, 'b> sandbox::SandboxContext for SandboxContext<'a, 'b> {
	fn invoke(
		&mut self,
		invoke_args_ptr: Pointer<u8>,
		invoke_args_len: WordSize,
		func_idx: SupervisorFuncIndex,
	) -> Result<i64> {
		let mut ret_vals = [Val::null()];
		let result = self.dispatch_thunk.call(
			&mut self.host_context.caller,
			&[
				Val::I32(u32::from(invoke_args_ptr) as i32),
				Val::I32(invoke_args_len as i32),
				Val::I32(self.state as i32),
				Val::I32(usize::from(func_idx) as i32),
			],
			&mut ret_vals,
		);

		match result {
			Ok(()) =>
				if let Some(ret_val) = ret_vals[0].i64() {
					Ok(ret_val)
				} else {
					Err("Supervisor function returned unexpected result!".into())
				},
			Err(err) => Err(err.to_string().into()),
		}
	}

	fn supervisor_context(&mut self) -> &mut dyn FunctionContext {
		self.host_context
	}
}
