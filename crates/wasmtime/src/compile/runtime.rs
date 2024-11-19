use crate::compile::HashedEngineCompileEnv;
#[cfg(feature = "component-model")]
use crate::component::Component;
use crate::prelude::*;
use crate::runtime::vm::MmapVec;
use crate::{CodeBuilder, CodeMemory, Engine, Module};
use object::write::WritableBuffer;
use std::sync::Arc;
use wasmtime_environ::{FinishedObject, ObjectBuilder, ObjectKind};

impl<'a> CodeBuilder<'a> {
    fn compile_cached<T>(
        &self,
        build_artifacts: fn(&Engine, &[u8], Option<&[u8]>) -> Result<(MmapVecWrapper, Option<T>)>,
    ) -> Result<(Arc<CodeMemory>, Option<T>)> {
        let wasm = self.get_wasm()?;
        let dwarf_package = self.get_dwarf_package();

        self.engine
            .check_compatible_with_native_host()
            .context("compilation settings are not compatible with the native host")?;

        #[cfg(feature = "cache")]
        {
            let state = (
                HashedEngineCompileEnv(self.engine),
                &wasm,
                &dwarf_package,
                // Don't hash this as it's just its own "pure" function pointer.
                NotHashed(build_artifacts),
            );
            let (code, info_and_types) =
                wasmtime_cache::ModuleCacheEntry::new("wasmtime", self.engine.cache_config())
                    .get_data_raw(
                        &state,
                        // Cache miss, compute the actual artifacts
                        |(engine, wasm, dwarf_package, build_artifacts)| -> Result<_> {
                            let (mmap, info) =
                                (build_artifacts.0)(engine.0, wasm, dwarf_package.as_deref())?;
                            let code = publish_mmap(mmap.0)?;
                            Ok((code, info))
                        },
                        // Implementation of how to serialize artifacts
                        |(_engine, _wasm, _, _), (code, _info_and_types)| {
                            Some(code.mmap().to_vec())
                        },
                        // Cache hit, deserialize the provided artifacts
                        |(engine, wasm, _, _), serialized_bytes| {
                            let kind = if wasmparser::Parser::is_component(&wasm) {
                                ObjectKind::Component
                            } else {
                                ObjectKind::Module
                            };
                            let code = engine.0.load_code_bytes(&serialized_bytes, kind).ok()?;
                            Some((code, None))
                        },
                    )?;
            return Ok((code, info_and_types));
        }

        #[cfg(not(feature = "cache"))]
        {
            let (mmap, info_and_types) =
                build_artifacts(self.engine, &wasm, dwarf_package.as_deref())?;
            let code = publish_mmap(mmap.0)?;
            return Ok((code, info_and_types));
        }

        struct NotHashed<T>(T);

        impl<T> std::hash::Hash for NotHashed<T> {
            fn hash<H: std::hash::Hasher>(&self, _hasher: &mut H) {}
        }
    }

    /// Same as [`CodeBuilder::compile_module_serialized`] except that a
    /// [`Module`](crate::Module) is produced instead.
    ///
    /// Note that this method will cache compilations if the `cache` feature is
    /// enabled and turned on in [`Config`](crate::Config).
    pub fn compile_module(&self) -> Result<Module> {
        let (code, info_and_types) = self.compile_cached(super::build_artifacts)?;
        Module::from_parts(self.engine, code, info_and_types)
    }

    /// Same as [`CodeBuilder::compile_module`] except that it compiles a
    /// [`Component`] instead of a module.
    #[cfg(feature = "component-model")]
    pub fn compile_component(&self) -> Result<Component> {
        let (code, artifacts) = self.compile_cached(super::build_component_artifacts)?;
        Component::from_parts(self.engine, code, artifacts)
    }
}

fn publish_mmap(mmap: MmapVec) -> Result<Arc<CodeMemory>> {
    let mut code = CodeMemory::new(mmap)?;
    code.publish()?;
    Ok(Arc::new(code))
}

pub(crate) struct MmapVecWrapper(pub MmapVec);

impl FinishedObject for MmapVecWrapper {
    fn finish_object(obj: ObjectBuilder<'_>) -> Result<Self> {
        let mut result = ObjectMmap::default();
        return match obj.finish(&mut result) {
            Ok(()) => {
                assert!(result.mmap.is_some(), "no reserve");
                let mmap = result.mmap.expect("reserve not called");
                assert_eq!(mmap.len(), result.len);
                Ok(MmapVecWrapper(mmap))
            }
            Err(e) => match result.err.take() {
                Some(original) => Err(original.context(e)),
                None => Err(e.into()),
            },
        };

        /// Helper struct to implement the `WritableBuffer` trait from the `object`
        /// crate.
        ///
        /// This enables writing an object directly into an mmap'd memory so it's
        /// immediately usable for execution after compilation. This implementation
        /// relies on a call to `reserve` happening once up front with all the needed
        /// data, and the mmap internally does not attempt to grow afterwards.
        #[derive(Default)]
        struct ObjectMmap {
            mmap: Option<MmapVec>,
            len: usize,
            err: Option<Error>,
        }

        impl WritableBuffer for ObjectMmap {
            fn len(&self) -> usize {
                self.len
            }

            fn reserve(&mut self, additional: usize) -> Result<(), ()> {
                assert!(self.mmap.is_none(), "cannot reserve twice");
                self.mmap = match MmapVec::with_capacity(additional) {
                    Ok(mmap) => Some(mmap),
                    Err(e) => {
                        self.err = Some(e);
                        return Err(());
                    }
                };
                Ok(())
            }

            fn resize(&mut self, new_len: usize) {
                // Resizing always appends 0 bytes and since new mmaps start out as 0
                // bytes we don't actually need to do anything as part of this other
                // than update our own length.
                if new_len <= self.len {
                    return;
                }
                self.len = new_len;
            }

            fn write_bytes(&mut self, val: &[u8]) {
                let mmap = self.mmap.as_mut().expect("write before reserve");
                // SAFETY: the `mmap` has not be made readonly yet so it should
                // be safe to mutate it.
                unsafe {
                    mmap.as_mut_slice()[self.len..][..val.len()].copy_from_slice(val);
                }
                self.len += val.len();
            }
        }
    }
}
