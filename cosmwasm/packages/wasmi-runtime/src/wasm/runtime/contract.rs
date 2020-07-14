use bech32::{FromBase32, ToBase32};
use log::*;
use wasmi::{Error as InterpreterError, MemoryInstance, MemoryRef, ModuleRef, RuntimeValue, Trap};

use enclave_ffi_types::Ctx;

use crate::consts::BECH32_PREFIX_ACC_ADDR;
use crate::crypto::Ed25519PublicKey;
use crate::wasm::contract_validation::ContractKey;
use crate::wasm::db::{read_encrypted_key, remove_encrypted_key, write_encrypted_key};
use crate::wasm::errors::WasmEngineError;
use crate::wasm::runtime::traits::WasmiApi;
use crate::wasm::{query_chain::encrypt_and_query_chain, types::IoNonce};

/// SecretContract maps function index to implementation
/// When instantiating a module we give it the SecretNetworkImportResolver resolver
/// When invoking a function inside the module we give it this runtime which is the actual functions implementation ()
pub struct ContractInstance {
    pub context: Ctx,
    pub memory: MemoryRef,
    pub gas_limit: u64,
    /// Gas used by wasmi
    pub gas_used: u64,
    /// Gas used by external services. This is tracked separately so we don't double-charge for external services later.
    pub gas_used_externally: u64,
    pub contract_key: ContractKey,
    pub module: ModuleRef,
    pub user_nonce: IoNonce,
    pub user_public_key: Ed25519PublicKey,
}

impl ContractInstance {
    fn get_memory(&self) -> &MemoryInstance {
        &*self.memory
    }

    pub fn new(
        context: Ctx,
        module: ModuleRef,
        gas_limit: u64,
        contract_key: ContractKey,
        user_nonce: IoNonce,
        user_public_key: Ed25519PublicKey,
    ) -> Self {
        let memory = (&*module)
            .export_by_name("memory")
            .expect("Module expected to have 'memory' export")
            .as_memory()
            .cloned()
            .expect("'memory' export should be of memory type");

        Self {
            context,
            memory,
            gas_limit,
            gas_used: 0,
            gas_used_externally: 0,
            contract_key,
            module,
            user_nonce,
            user_public_key,
        }
    }

    /// extract_vector extracts a vector from the wasm memory space
    pub fn extract_vector(&self, vec_ptr_ptr: u32) -> Result<Vec<u8>, WasmEngineError> {
        self.extract_vector_inner(vec_ptr_ptr).map_err(|err| {
            error!(
                "error while trying to read the buffer at {:?} : {:?}",
                vec_ptr_ptr, err
            );
            WasmEngineError::MemoryReadError
        })
    }

    fn extract_vector_inner(&self, vec_ptr_ptr: u32) -> Result<Vec<u8>, InterpreterError> {
        let ptr: u32 = self.get_memory().get_value(vec_ptr_ptr)?;

        if ptr == 0 {
            return Err(InterpreterError::Memory(String::from(
                "Trying to read from null pointer in WASM memory",
            )));
        }

        let len: u32 = self.get_memory().get_value(vec_ptr_ptr + 8)?;

        self.get_memory().get(ptr, len as usize)
    }

    pub fn allocate(&mut self, len: u32) -> Result<u32, WasmEngineError> {
        self.allocate_inner(len).map_err(|err| {
            error!("Failed to allocate {} bytes in wasm: {}", len, err);
            WasmEngineError::MemoryAllocationError
        })
    }

    fn allocate_inner(&mut self, len: u32) -> Result<u32, InterpreterError> {
        match self.module.clone().invoke_export(
            "allocate",
            &[RuntimeValue::I32(len as i32)],
            self,
        )? {
            Some(RuntimeValue::I32(0)) => Err(InterpreterError::Memory(String::from(
                "Allocate returned null pointer from WASM",
            ))),
            Some(RuntimeValue::I32(offset)) => Ok(offset as u32),
            other => Err(InterpreterError::Value(format!(
                "allocate method returned value which wasn't u32: {:?}",
                other
            ))),
        }
    }

    pub fn write_to_allocated_memory(
        &mut self,
        buffer: &[u8],
        ptr_to_region_in_wasm_vm: u32,
    ) -> Result<u32, WasmEngineError> {
        self.write_to_allocated_memory_inner(buffer, ptr_to_region_in_wasm_vm)
            .map_err(|err| {
                error!(
                    "error while trying to write the buffer {:?} to the destination buffer at {:?} : {:?}",
                    buffer, ptr_to_region_in_wasm_vm, err
                );
                WasmEngineError::MemoryWriteError
            })
    }

    fn write_to_allocated_memory_inner(
        &mut self,
        buffer: &[u8],
        ptr_to_region_in_wasm_vm: u32,
    ) -> Result<u32, InterpreterError> {
        // WASM pointers are pointers to "Region"
        // Region is a struct that looks like this:
        // ptr_to_region -> | 4byte = buffer_addr | 4bytes = buffer_cap | 4bytes = buffer_len |

        // extract the buffer pointer from the region
        let buffer_addr_in_wasm: u32 = self
            .get_memory()
            .get_value::<u32>(ptr_to_region_in_wasm_vm)?;

        if buffer_addr_in_wasm == 0 {
            return Err(InterpreterError::Memory(String::from(
                "Trying to write to null pointer in WASM memory",
            )));
        }

        let buffer_cap_in_wasm: u32 = self
            .get_memory()
            .get_value::<u32>(ptr_to_region_in_wasm_vm + 4)?;

        if buffer_cap_in_wasm < buffer.len() as u32 {
            return Err(InterpreterError::Memory(format!(
                "Tried to write {} bytes but only got {} bytes in destination buffer",
                buffer.len(),
                buffer_cap_in_wasm
            )));
        }

        self.get_memory().set(buffer_addr_in_wasm, buffer)?;

        self.get_memory()
            .set_value::<u32>(ptr_to_region_in_wasm_vm + 8, buffer.len() as u32)?;

        // return the WASM pointer
        Ok(ptr_to_region_in_wasm_vm)
    }

    pub fn write_to_memory(&mut self, buffer: &[u8]) -> Result<u32, WasmEngineError> {
        // allocate return a pointer to a region
        let ptr_to_region_in_wasm_vm = self.allocate(buffer.len() as u32)?;
        self.write_to_allocated_memory(buffer, ptr_to_region_in_wasm_vm)
    }

    /// Track gas used inside wasmi
    fn use_gas(&mut self, gas_amount: u64) -> Result<(), WasmEngineError> {
        self.gas_used = self.gas_used.saturating_add(gas_amount);
        self.check_gas_usage()
    }

    /// Track gas used by external services (e.g. storage)
    fn use_gas_externally(&mut self, gas_amount: u64) -> Result<(), WasmEngineError> {
        self.gas_used_externally = self.gas_used_externally.saturating_add(gas_amount);
        self.check_gas_usage()
    }

    fn check_gas_usage(&self) -> Result<(), WasmEngineError> {
        // Check if new amount is bigger than gas limit
        // If is above the limit, halt execution
        if self.is_gas_depleted() {
            warn!(
                "Out of gas! Gas limit: {}, gas used: {}, gas used externally: {}",
                self.gas_limit, self.gas_used, self.gas_used_externally
            );
            Err(WasmEngineError::OutOfGas)
        } else {
            Ok(())
        }
    }

    fn is_gas_depleted(&self) -> bool {
        self.gas_limit < self.gas_used.saturating_add(self.gas_used_externally)
    }
}

impl WasmiApi for ContractInstance {
    /// Args:
    /// 1. "key" to read from Tendermint (buffer of bytes)
    /// key is a pointer to a region "struct" of "pointer" and "length"
    /// A Region looks like { ptr: u32, len: u32 }
    fn read_db_index(&mut self, state_key_ptr_ptr: i32) -> Result<Option<RuntimeValue>, Trap> {
        let state_key_name = self
            .extract_vector(state_key_ptr_ptr as u32)
            .map_err(|err| {
                error!("read_db() error while trying to read state_key_name from wasm memory");
                err
            })?;

        trace!(
            "read_db() was called from WASM code with state_key_name: {:?}",
            String::from_utf8_lossy(&state_key_name)
        );

        // Call read_db (this bubbles up to Tendermint via ocalls and FFI to Go code)
        // This returns the value from Tendermint
        let (value, gas_used) =
            read_encrypted_key(&state_key_name, &self.context, &self.contract_key)?;
        self.use_gas_externally(gas_used)?;

        let value = match value {
            None => return Ok(Some(RuntimeValue::I32(0))),
            Some(value) => value,
        };

        trace!(
            "read_db() got value with len {}: '{:?}'",
            value.len(),
            value
        );

        let ptr_to_region_in_wasm_vm = self.write_to_memory(&value).map_err(|err| {
            error!(
                "read_db() error while trying to allocate {} bytes for the value",
                value.len(),
            );
            err
        })?;

        // Return pointer to the allocated buffer with the value written to it
        Ok(Some(RuntimeValue::I32(ptr_to_region_in_wasm_vm as i32)))
    }

    /// Args:
    /// 1. "key" to delete from Tendermint (buffer of bytes)
    /// key is a pointer to a region "struct" of "pointer" and "length"
    /// A Region looks like { ptr: u32, len: u32 }
    fn remove_db_index(&mut self, state_key_ptr_ptr: i32) -> Result<Option<RuntimeValue>, Trap> {
        let state_key_name = self
            .extract_vector(state_key_ptr_ptr as u32)
            .map_err(|err| {
                error!("remove_db() error while trying to read state_key_name from wasm memory");
                err
            })?;

        trace!(
            "remove_db() was called from WASM code with state_key_name: {:?}",
            String::from_utf8_lossy(&state_key_name)
        );

        // Call remove_db (this bubbles up to Tendermint via ocalls and FFI to Go code)
        let gas_used = remove_encrypted_key(&state_key_name, &self.context, &self.contract_key)?;
        self.use_gas_externally(gas_used)?;

        Ok(None)
    }

    /// Args:
    /// 1. "key" to write to Tendermint (buffer of bytes)
    /// 2. "value" to write to Tendermint (buffer of bytes)
    /// Both of them are pointers to a region "struct" of "pointer" and "length"
    /// Lets say Region looks like { ptr: u32, len: u32 }
    fn write_db_index(
        &mut self,
        state_key_ptr_ptr: i32,
        value_ptr_ptr: i32,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let state_key_name = self
            .extract_vector(state_key_ptr_ptr as u32)
            .map_err(|err| {
                error!("write_db() error while trying to read state_key_name from wasm memory");
                err
            })?;
        let value = self.extract_vector(value_ptr_ptr as u32).map_err(|err| {
            error!("write_db() error while trying to read value from wasm memory");
            err
        })?;

        trace!(
            "write_db() was called from WASM code with state_key_name: {:?} value: {:?}",
            String::from_utf8_lossy(&state_key_name),
            String::from_utf8_lossy(&value),
        );

        let used_gas =
            write_encrypted_key(&state_key_name, &value, &self.context, &self.contract_key)
                .map_err(|err| {
                    error!(
                        "write_db() error while trying to write the value to state: {:?}",
                        err
                    );
                    err
                })?;
        self.use_gas_externally(used_gas)?;

        Ok(None)
    }

    /// Args:
    /// 1. "human" to convert to canonical address (string)
    /// 2. "canonical" a buffer to write the result into (buffer of bytes)
    /// Both of them are pointers to a region "struct" of "pointer" and "length"
    /// A Region looks like { ptr: u32, len: u32 }
    fn canonicalize_address_index(
        &mut self,
        human_ptr_ptr: i32,
        canonical_ptr_ptr: i32,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let human = self.extract_vector(human_ptr_ptr as u32).map_err(|err| {
            error!(
                "canonicalize_address() error while trying to read human address from wasm memory"
            );
            err
        })?;

        trace!(
            "canonicalize_address() was called from WASM code with {:?}",
            String::from_utf8_lossy(&human)
        );

        // Turn Vec<u8> to str
        let mut human_addr_str = match std::str::from_utf8(&human) {
            Err(err) => {
                error!(
                    "canonicalize_address() error while trying to parse human address from bytes to string: {:?}",
                    err
                );
                return Ok(Some(RuntimeValue::I32(-1)));
            }
            Ok(x) => x,
        };

        human_addr_str = human_addr_str.trim();
        if human_addr_str.is_empty() {
            return Ok(Some(RuntimeValue::I32(-2)));
        }

        let (decoded_prefix, data) = match bech32::decode(&human_addr_str) {
            Err(err) => {
                error!(
                    "canonicalize_address() error while trying to decode human address {:?} as bech32: {:?}",
                    human_addr_str, err
                );
                return Ok(Some(RuntimeValue::I32(-3)));
            }
            Ok(x) => x,
        };

        if decoded_prefix != BECH32_PREFIX_ACC_ADDR {
            warn!(
                "canonicalize_address() wrong prefix {:?} (expected {:?}) while decoding human address {:?} as bech32",
                decoded_prefix,
                BECH32_PREFIX_ACC_ADDR,
                human_addr_str
            );
            return Ok(Some(RuntimeValue::I32(-4)));
        }

        let canonical = match Vec::<u8>::from_base32(&data) {
            Err(err) => {
                // Assaf: From reading https://docs.rs/bech32/0.7.2/src/bech32/lib.rs.html#607
                // and https://docs.rs/bech32/0.7.2/src/bech32/lib.rs.html#228 I don't think this can fail that way
                warn!(
                    "canonicalize_address() error while trying to decode bytes from base32 {:?}: {:?}",
                    data, err
                );
                return Ok(Some(RuntimeValue::I32(-5)));
            }
            Ok(x) => x,
        };

        self.write_to_allocated_memory(&canonical, canonical_ptr_ptr as u32)
            .map_err(|err| {
                error!(
                    "canonicalize_address() error while trying to write the answer {:?} to the destination buffer",
                    canonical,
                );
                err
            })?;

        // return 0 == ok
        Ok(Some(RuntimeValue::I32(0)))
    }

    /// Args:
    /// 1. "canonical" to convert to human address (buffer of bytes)
    /// 2. "human" a buffer to write the result (humanized string) into (buffer of bytes)
    /// Both of them are pointers to a region "struct" of "pointer" and "length"
    /// A Region looks like { ptr: u32, len: u32 }
    fn humanize_address_index(
        &mut self,
        canonical_ptr_ptr: i32,
        human_ptr_ptr: i32,
    ) -> Result<Option<RuntimeValue>, Trap> {
        let canonical = self
            .extract_vector(canonical_ptr_ptr as u32)
            .map_err(|err| {
                error!(
                    "humanize_address() error while trying to read canonical address from wasm memory",
                );
                err
            })?;

        trace!(
            "humanize_address() was called from WASM code with {:?}",
            canonical
        );

        let human_addr_str = match bech32::encode(BECH32_PREFIX_ACC_ADDR, canonical.to_base32()) {
            Err(err) => {
                // Assaf: IMO This can never fail. From looking at bech32::encode, it only fails
                // because input prefix issues. For us the prefix is always "secert" which is valid.
                error!("humanize_address() error while trying to encode canonical address {:?} to human: {:?}",  canonical, err);
                return Ok(Some(RuntimeValue::I32(-1)));
            }
            Ok(x) => x,
        };

        let human_bytes = human_addr_str.into_bytes();

        self.write_to_allocated_memory(&human_bytes, human_ptr_ptr as u32)
            .map_err(|err| {
                error!(
                    "humanize_address() error while trying to write the answer {:?} to the destination buffer",
                    human_bytes,
                );
                err
            })?;

        // return 0 == ok
        Ok(Some(RuntimeValue::I32(0)))
    }

    // stub, for now
    fn query_chain_index(&mut self, query_ptr_ptr: i32) -> Result<Option<RuntimeValue>, Trap> {
        let query_buffer = self.extract_vector(query_ptr_ptr as u32).map_err(|err| {
            error!("query_chain() error while trying to read canonical address from wasm memory",);
            err
        })?;

        trace!(
            "query_chain() was called from WASM code with {:?}",
            String::from_utf8_lossy(&query_buffer)
        );

        // Call query_chain (this bubbles up to x/compute via ocalls and FFI to Go code)
        // Returns the value from x/compute
        let (result, gas_used) = encrypt_and_query_chain(
            &query_buffer,
            &self.context,
            self.user_nonce,
            self.user_public_key,
        )?;
        self.use_gas_externally(gas_used)?;

        let result = match result {
            None => return Ok(Some(RuntimeValue::I32(0))), // Is this supposed to be 0 or Err?
            Some(result) => result,
        };

        let ptr_to_region_in_wasm_vm =   self.write_to_memory(&result)
            .map_err(|err| {
                error!(
                    "query_chain() error while trying to allocate and write the answer {:?} to the WASM VM",
                    result,
                );
                err
            })?;

        // Return pointer to the allocated buffer with the value written to it
        Ok(Some(RuntimeValue::I32(ptr_to_region_in_wasm_vm as i32)))
    }

    fn gas_index(&mut self, gas_amount: i32) -> Result<Option<RuntimeValue>, Trap> {
        self.use_gas(gas_amount as u64)?;
        Ok(None)
    }
}
