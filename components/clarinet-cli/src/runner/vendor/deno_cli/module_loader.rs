// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use super::emit::emit_parsed_source;
use super::emit::TsTypeLib;
use super::graph_util::ModuleEntry;
use super::proc_state::ProcState;
use super::text_encoding::code_without_source_map;
use super::text_encoding::source_map_from_code;

use super::super::deno_runtime::permissions::Permissions;
use deno_ast::MediaType;
use deno_core::anyhow::anyhow;
use deno_core::error::AnyError;
use deno_core::futures::future::FutureExt;
use deno_core::futures::Future;
use deno_core::resolve_url;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::OpState;
use deno_core::SourceMapGetter;
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::str;

struct ModuleCodeSource {
    pub code: String,
    pub found_url: ModuleSpecifier,
    pub media_type: MediaType,
}

pub struct CliModuleLoader {
    pub lib: TsTypeLib,
    /// The initial set of permissions used to resolve the static imports in the
    /// worker. They are decoupled from the worker (dynamic) permissions since
    /// read access errors must be raised based on the parent thread permissions.
    pub root_permissions: Permissions,
    pub ps: ProcState,
}

impl CliModuleLoader {
    pub fn new(ps: ProcState) -> Rc<Self> {
        Rc::new(CliModuleLoader {
            lib: ps.options.ts_type_lib_window(),
            root_permissions: Permissions::allow_all(),
            ps,
        })
    }

    pub fn new_for_worker(ps: ProcState, permissions: Permissions) -> Rc<Self> {
        Rc::new(CliModuleLoader {
            lib: ps.options.ts_type_lib_worker(),
            root_permissions: permissions,
            ps,
        })
    }

    fn load_prepared_module(
        &self,
        specifier: &ModuleSpecifier,
    ) -> Result<ModuleCodeSource, AnyError> {
        let graph_data = self.ps.graph_data.read();
        let found_url = graph_data.follow_redirect(specifier);
        match graph_data.get(&found_url) {
            Some(ModuleEntry::Module {
                code,
                media_type,
                maybe_parsed_source,
                ..
            }) => {
                let code = match media_type {
                    MediaType::JavaScript
                    | MediaType::Unknown
                    | MediaType::Cjs
                    | MediaType::Mjs
                    | MediaType::Json => {
                        if let Some(source) = graph_data.get_cjs_esm_translation(specifier) {
                            source.to_owned()
                        } else {
                            code.to_string()
                        }
                    }
                    MediaType::Dts | MediaType::Dcts | MediaType::Dmts => "".to_string(),
                    MediaType::TypeScript
                    | MediaType::Mts
                    | MediaType::Cts
                    | MediaType::Jsx
                    | MediaType::Tsx => {
                        // get emit text
                        let parsed_source = maybe_parsed_source.as_ref().unwrap(); // should always be set
                        emit_parsed_source(
                            &self.ps.emit_cache,
                            &found_url,
                            parsed_source,
                            &self.ps.emit_options,
                            self.ps.emit_options_hash,
                        )?
                    }
                    MediaType::TsBuildInfo | MediaType::Wasm | MediaType::SourceMap => {
                        panic!("Unexpected media type {} for {}", media_type, found_url)
                    }
                };
                Ok(ModuleCodeSource {
                    code,
                    found_url,
                    media_type: *media_type,
                })
            }
            _ => Err(anyhow!(
                "Loading unprepared module: {}",
                specifier.to_string()
            )),
        }
    }
}

impl ModuleLoader for CliModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _is_main: bool,
    ) -> Result<ModuleSpecifier, AnyError> {
        self.ps.resolve(specifier, referrer)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dynamic: bool,
    ) -> Pin<Box<deno_core::ModuleSourceFuture>> {
        // NOTE: this block is async only because of `deno_core` interface
        // requirements; module was already loaded when constructing module graph
        // during call to `prepare_load` so we can load it synchronously.
        let result = self.load_prepared_module(specifier).map(|code_source| {
            let code = if self.ps.options.is_inspecting() {
                // we need the code with the source map in order for
                // it to work with --inspect or --inspect-brk
                code_source.code
            } else {
                // reduce memory and throw away the source map
                // because we don't need it
                code_without_source_map(code_source.code)
            };
            ModuleSource {
                code: code.into_bytes().into_boxed_slice(),
                module_url_specified: specifier.to_string(),
                module_url_found: code_source.found_url.to_string(),
                module_type: match code_source.media_type {
                    MediaType::Json => ModuleType::Json,
                    _ => ModuleType::JavaScript,
                },
            }
        });

        Box::pin(deno_core::futures::future::ready(result))
    }

    fn prepare_load(
        &self,
        op_state: Rc<RefCell<OpState>>,
        specifier: &ModuleSpecifier,
        _maybe_referrer: Option<String>,
        is_dynamic: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), AnyError>>>> {
        let specifier = specifier.clone();
        let ps = self.ps.clone();
        let state = op_state.borrow();

        let dynamic_permissions = state.borrow::<Permissions>().clone();
        let root_permissions = if is_dynamic {
            dynamic_permissions.clone()
        } else {
            self.root_permissions.clone()
        };
        let lib = self.lib;

        drop(state);

        async move {
            ps.prepare_module_load(
                vec![specifier],
                is_dynamic,
                lib,
                root_permissions,
                dynamic_permissions,
                false,
            )
            .await
        }
        .boxed_local()
    }
}

impl SourceMapGetter for CliModuleLoader {
    fn get_source_map(&self, file_name: &str) -> Option<Vec<u8>> {
        if let Ok(specifier) = resolve_url(file_name) {
            match specifier.scheme() {
                // we should only be looking for emits for schemes that denote external
                // modules, which the disk_cache supports
                "wasm" | "file" | "http" | "https" | "data" | "blob" => (),
                _ => return None,
            }
            if let Ok(source) = self.load_prepared_module(&specifier) {
                source_map_from_code(&source.code)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn get_source_line(&self, file_name: &str, line_number: usize) -> Option<String> {
        let graph_data = self.ps.graph_data.read();
        let specifier = graph_data.follow_redirect(&resolve_url(file_name).ok()?);
        let code = match graph_data.get(&specifier) {
            Some(ModuleEntry::Module { code, .. }) => code,
            _ => return None,
        };
        // Do NOT use .lines(): it skips the terminating empty line.
        // (due to internally using_terminator() instead of .split())
        let lines: Vec<&str> = code.split('\n').collect();
        if line_number >= lines.len() {
            Some(format!(
        "{} Couldn't format source line: Line {} is out of bounds (source may have changed at runtime)",
        super::super::deno_runtime::colors::yellow("Warning"), line_number + 1,
      ))
        } else {
            Some(lines[line_number].to_string())
        }
    }
}
