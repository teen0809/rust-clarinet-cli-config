// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use super::proc_state::ProcState;
use deno_core::Extension;

pub mod testing;

pub fn cli_exts(ps: ProcState) -> Vec<Extension> {
    vec![init_proc_state(ps)]
}

fn init_proc_state(ps: ProcState) -> Extension {
    Extension::builder()
        .state(move |state| {
            state.put(ps.clone());
            Ok(())
        })
        .build()
}
