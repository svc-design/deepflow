/*
 * Copyright (c) 2024 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use enum_dispatch::enum_dispatch;
use public::l7_protocol::{CustomProtocol, L7Protocol, L7ProtocolEnum};
use wasm::WasmLog;

use crate::{
    common::{
        flow::L7PerfStats,
        l7_protocol_log::{L7ParseResult, L7ProtocolParser, L7ProtocolParserInterface, ParseParam},
    },
    flow_generator::{protocol_logs::sql::ObfuscateCache, Result},
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use self::shared_obj::{get_so_parser, SoLog};
use self::{custom_wrap::CustomWrapLog, wasm::get_wasm_parser};

pub mod custom_wrap;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod shared_obj;
pub mod wasm;

cfg_if::cfg_if! {
if #[cfg(feature = "enterprise")] {
        pub mod custom_protocol_policy;
        pub use custom_protocol_policy::{get_policy_parser, CustomPolicyLog};
    }
}

#[enum_dispatch(L7ProtocolParserInterface)]
pub enum CustomLog {
    WasmLog(WasmLog),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SoLog(SoLog),
    #[cfg(feature = "enterprise")]
    CustomPolicyLog(CustomPolicyLog),
}

pub fn get_custom_log_parser(proto: CustomProtocol) -> L7ProtocolParser {
    L7ProtocolParser::Custom(CustomWrapLog {
        parser: Some(match proto {
            CustomProtocol::Wasm(p, s) => CustomLog::WasmLog(get_wasm_parser(p, s)),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            CustomProtocol::So(p, s) => CustomLog::SoLog(get_so_parser(p, s)),
            #[cfg(target_os = "windows")]
            CustomProtocol::So(_, _) => unimplemented!(),
            #[cfg(feature = "enterprise")]
            CustomProtocol::CustomPolicy(s) => CustomLog::CustomPolicyLog(get_policy_parser(s)),
            #[cfg(not(feature = "enterprise"))]
            CustomProtocol::CustomPolicy(_) => unimplemented!(),
        }),
    })
}

#[inline(always)]
fn all_plugin_log_parser() -> Vec<CustomLog> {
    vec![
        CustomLog::WasmLog(WasmLog::default()),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        CustomLog::SoLog(SoLog::default()),
        #[cfg(feature = "enterprise")]
        CustomLog::CustomPolicyLog(CustomPolicyLog::default()),
    ]
}
