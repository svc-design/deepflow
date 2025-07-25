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

mod comment_parser;
mod consts;

use std::cell::Cell;
use std::io::Read;
use std::str;

use flate2::bufread::ZlibDecoder;
use log::{debug, trace};
use serde::Serialize;

use super::{
    sql_check::{is_mysql, is_valid_sql, trim_head_comment_and_get_first_word},
    sql_obfuscate::attempt_obfuscation,
    ObfuscateCache,
};

use self::consts::*;
use crate::{
    common::{
        enums::IpProtocol,
        flow::{L7PerfStats, L7Protocol, PacketDirection},
        l7_protocol_info::{L7ProtocolInfo, L7ProtocolInfoInterface},
        l7_protocol_log::{L7ParseResult, L7ProtocolParserInterface, ParseParam},
        meta_packet::EbpfFlags,
    },
    config::handler::{L7LogDynamicConfig, LogParserConfig},
    flow_generator::{
        error,
        protocol_logs::{
            pb_adapter::{ExtendedInfo, L7ProtocolSendLog, L7Request, L7Response, TraceInfo},
            set_captured_byte, value_is_default, AppProtoHead, L7ResponseStatus, LogMessageType,
        },
    },
    utils::bytes,
};
use public::l7_protocol::L7ProtocolChecker;

const SERVER_STATUS_CODE_MIN: u16 = 1000;
const CLIENT_STATUS_CODE_MIN: u16 = 2000;
const CLIENT_STATUS_CODE_MAX: u16 = 2999;

#[derive(Debug)]
enum TruncationType {
    Packet,
    PacketHeader,
    PacketPayload(MysqlHeader),

    CompressedHeader,
    CompressedPacket,
    CompressedPacketHeader,
    CompressedPacketPayload(MysqlHeader),

    Greeting,
    Login,

    Request,
    Response,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("no packet found in payload")]
    NoPacket,
    #[error("ignored packet with header: {0:?}")]
    IgnoredPacket(MysqlHeader),
    #[error("truncated at {0:?}")]
    Truncated(TruncationType),
    #[error("compressed packet not parsed")]
    CompressedPacketNotParsed,

    #[error("invalid sql statement")]
    InvalidSqlStatement,
    #[error("invalid login info: {0}")]
    InvalidLoginInfo(&'static str),
    #[error("command {0} not supported")]
    CommandNotSupported(u8),

    #[error("invalid error code {0}")]
    InvalidResponseErrorCode(u16),
    #[error("invalid response error message")]
    InvalidResponseErrorMessage,
}

impl From<Error> for error::Error {
    fn from(e: Error) -> Self {
        error::Error::L7LogParseFailed {
            proto: L7Protocol::MySQL,
            reason: e.to_string().into(),
        }
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Serialize, Debug, Default, Clone)]
pub struct MysqlInfo {
    msg_type: LogMessageType,
    #[serde(skip)]
    is_tls: bool,

    // Server Greeting
    #[serde(rename = "version", skip_serializing_if = "value_is_default")]
    pub protocol_version: u8,
    // request
    #[serde(rename = "request_type")]
    pub command: u8,
    #[serde(rename = "request_resource", skip_serializing_if = "value_is_default")]
    pub context: String,
    // response
    pub response_code: u8,
    #[serde(skip)]
    pub error_code: Option<i32>,
    #[serde(rename = "sql_affected_rows", skip_serializing_if = "value_is_default")]
    pub affected_rows: u64,
    #[serde(
        rename = "response_execption",
        skip_serializing_if = "value_is_default"
    )]
    pub error_message: String,
    #[serde(rename = "response_status")]
    pub status: L7ResponseStatus,

    rrt: u64,
    // This field is extracted in the following message:
    // 1. Response message corresponding to COM_STMT_PREPARE request
    // 2. COM_STMT_EXECUTE request message
    statement_id: u32,

    captured_request_byte: u32,
    captured_response_byte: u32,

    trace_id: Option<String>,
    span_id: Option<String>,

    #[serde(skip)]
    is_on_blacklist: bool,
}

impl L7ProtocolInfoInterface for MysqlInfo {
    fn session_id(&self) -> Option<u32> {
        None
    }

    fn merge_log(&mut self, other: &mut L7ProtocolInfo) -> error::Result<()> {
        if let L7ProtocolInfo::MysqlInfo(other) = other {
            self.merge(other);
        }
        Ok(())
    }

    fn app_proto_head(&self) -> Option<AppProtoHead> {
        Some(AppProtoHead {
            proto: L7Protocol::MySQL,
            msg_type: self.msg_type,
            rrt: self.rrt,
        })
    }

    fn is_tls(&self) -> bool {
        self.is_tls
    }

    fn get_request_resource_length(&self) -> usize {
        self.context.len()
    }

    fn is_on_blacklist(&self) -> bool {
        self.is_on_blacklist
    }

    // all unmerged responses are skipped because segmented responses can produce multiple OK responses
    fn skip_send(&self) -> bool {
        self.msg_type == LogMessageType::Response
    }
}

impl MysqlInfo {
    pub fn merge(&mut self, other: &mut Self) {
        if self.protocol_version == 0 {
            self.protocol_version = other.protocol_version
        }
        if other.is_on_blacklist {
            self.is_on_blacklist = other.is_on_blacklist;
        }
        match other.msg_type {
            LogMessageType::Request => {
                self.command = other.command;
                std::mem::swap(&mut self.context, &mut other.context);
                self.captured_request_byte = other.captured_request_byte;
            }
            LogMessageType::Response => {
                self.response_code = other.response_code;
                self.affected_rows = other.affected_rows;
                std::mem::swap(&mut self.error_message, &mut other.error_message);
                self.status = other.status;
                if self.error_code.is_none() {
                    self.error_code = other.error_code;
                }
                if self.command == COM_STMT_PREPARE && other.statement_id > 0 {
                    self.statement_id = other.statement_id;
                } else {
                    self.statement_id = 0;
                }
                self.captured_response_byte = other.captured_response_byte;
            }
            _ => {}
        }
    }

    pub fn get_command_str(&self) -> &'static str {
        let command = [
            "", // command 0 is resp, ignore
            "COM_QUIT",
            "COM_INIT_DB",
            "COM_QUERY",
            "COM_FIELD_LIST",
            "COM_CREATE_DB",
            "COM_DROP_DB",
            "COM_REFRESH",
            "COM_SHUTDOWN",
            "COM_STATISTICS",
            "COM_PROCESS_INFO",
            "COM_CONNECT",
            "COM_PROCESS_KILL",
            "COM_DEBUG",
            "COM_PING",
            "COM_TIME",
            "COM_DELAYED_INSERT",
            "COM_CHANGE_USER",
            "COM_BINLOG_DUMP",
            "COM_TABLE_DUMP",
            "COM_CONNECT_OUT",
            "COM_REGISTER_SLAVE",
            "COM_STMT_PREPARE",
            "COM_STMT_EXECUTE",
            "COM_STMT_SEND_LONG_DATA",
            "COM_STMT_CLOSE",
            "COM_STMT_RESET",
            "COM_SET_OPTION",
            "COM_STMT_FETCH",
            "COM_DAEMON",
            "COM_BINLOG_DUMP_GTID",
            "COM_RESET_CONNECTION",
        ];
        match self.command {
            0x00..=0x1f => command[self.command as usize],
            _ => "",
        }
    }

    fn request_string(
        &mut self,
        config: Option<&LogParserConfig>,
        payload: &[u8],
        obfuscate_cache: &Option<ObfuscateCache>,
    ) -> Result<()> {
        let payload = mysql_string(payload);
        if (self.command == COM_QUERY || self.command == COM_STMT_PREPARE) && !is_mysql(payload) {
            return Err(Error::InvalidSqlStatement);
        };
        let Ok(sql_string) = str::from_utf8(payload) else {
            return Err(Error::InvalidSqlStatement);
        };
        if let Some(c) = config {
            self.extract_trace_and_span_id(&c.l7_log_dynamic, sql_string);
        }
        let context = match attempt_obfuscation(obfuscate_cache, payload) {
            Some(mut m) => {
                let valid_len = match str::from_utf8(&m) {
                    Ok(_) => m.len(),
                    Err(e) => e.valid_up_to(),
                };
                m.truncate(valid_len);
                unsafe {
                    // SAFTY: str in m is checked to be valid utf8 up to `valid_len`
                    String::from_utf8_unchecked(m)
                }
            }
            _ => String::from_utf8_lossy(payload).to_string(),
        };
        self.context = context;
        Ok(())
    }

    // extra trace id from comment like # TraceID: xxxxxxxxxxxxxxx
    fn extract_trace_and_span_id(&mut self, config: &L7LogDynamicConfig, sql: &str) {
        if config.trace_types.is_empty() && config.span_types.is_empty() {
            return;
        }
        debug!("extract id from sql {sql}");
        'outer: for comment in comment_parser::MysqlCommentParserIter::new(sql) {
            trace!("comment={comment}");
            for (key, value) in KvExtractor::new(comment) {
                trace!("key={key} value={value}");
                for tt in config.trace_types.iter() {
                    if tt.check(key) {
                        self.trace_id = tt.decode_trace_id(value).map(|s| s.to_string());
                        break;
                    }
                }
                for st in config.span_types.iter() {
                    if st.check(key) {
                        self.span_id = st.decode_span_id(value).map(|s| s.to_string());
                        break;
                    }
                }
                if self.trace_id.is_some() && config.span_types.is_empty()
                    || self.span_id.is_some() && config.trace_types.is_empty()
                    || self.trace_id.is_some() && self.span_id.is_some()
                {
                    break 'outer;
                }
            }
        }
        debug!(
            "extracted trace_id={:?} span_id={:?}",
            self.trace_id, self.span_id
        );
    }

    fn statement_id(&mut self, payload: &[u8]) {
        if payload.len() >= STATEMENT_ID_LEN {
            self.statement_id = bytes::read_u32_le(payload)
        }
    }

    fn set_is_on_blacklist(&mut self, config: &LogParserConfig) {
        if let Some(t) = config.l7_log_blacklist_trie.get(&L7Protocol::MySQL) {
            self.is_on_blacklist = t.request_resource.is_on_blacklist(&self.context)
                || t.request_type.is_on_blacklist(self.get_command_str());
        }
    }
}

impl From<MysqlInfo> for L7ProtocolSendLog {
    fn from(f: MysqlInfo) -> Self {
        let flags = if f.is_tls {
            EbpfFlags::TLS.bits()
        } else {
            EbpfFlags::NONE.bits()
        };
        let log = L7ProtocolSendLog {
            captured_request_byte: f.captured_request_byte,
            captured_response_byte: f.captured_response_byte,
            version: if f.protocol_version == 0 {
                None
            } else {
                Some(f.protocol_version.to_string())
            },

            row_effect: if f.command == COM_QUERY {
                trim_head_comment_and_get_first_word(f.context.as_bytes(), 8)
                    .map(|first| {
                        if is_valid_sql(first, &["INSERT", "UPDATE", "DELETE"]) {
                            f.affected_rows as u32
                        } else {
                            0
                        }
                    })
                    .unwrap_or_default()
            } else {
                0
            },
            req: L7Request {
                req_type: String::from(f.get_command_str()),
                resource: f.context,
                ..Default::default()
            },
            resp: L7Response {
                status: f.status,
                code: f.error_code,
                exception: f.error_message,
                ..Default::default()
            },
            ext_info: Some(ExtendedInfo {
                request_id: f.statement_id.into(),
                ..Default::default()
            }),
            trace_info: if f.trace_id.is_some() || f.span_id.is_some() {
                Some(TraceInfo {
                    trace_id: f.trace_id,
                    span_id: f.span_id,
                    ..Default::default()
                })
            } else {
                None
            },
            flags,
            ..Default::default()
        };
        return log;
    }
}

thread_local! {
    static DECODE_BUFFER: Cell<Option<Vec<u8>>> = Cell::new(None);
}

fn take_buffer() -> Vec<u8> {
    DECODE_BUFFER.with(|c| c.take().unwrap_or_default())
}

fn give_buffer(buffer: Vec<u8>) {
    DECODE_BUFFER.with(|c| c.replace(Some(buffer)));
}

#[derive(Default)]
pub struct MysqlLog {
    pub protocol_version: u8,
    perf_stats: Option<L7PerfStats>,
    obfuscate_cache: Option<ObfuscateCache>,

    // This field is extracted in the COM_STMT_PREPARE request and calculate based on SQL statements
    pc: ParameterCounter,
    has_request: bool,
    has_login: bool,

    last_is_on_blacklist: bool,

    // if compression is enabled, both requests and responses will have compression header
    has_compressed_header: Option<bool>,
}

impl L7ProtocolParserInterface for MysqlLog {
    fn check_payload(&mut self, payload: &[u8], param: &ParseParam) -> bool {
        if !param.ebpf_type.is_raw_protocol() || param.l4_protocol != IpProtocol::TCP {
            return false;
        }

        if self.has_compressed_header.is_none() {
            if let Err(_) = self.check_compressed_header(payload) {
                return false;
            }
        }

        let mut decompress_buffer = take_buffer();
        let ret = Self::check(
            param.parse_config,
            &mut decompress_buffer,
            self.has_compressed_header.unwrap(),
            payload,
        );
        give_buffer(decompress_buffer);

        ret
    }

    fn parse_payload(
        &mut self,
        payload: &[u8],
        param: &ParseParam,
    ) -> error::Result<L7ParseResult> {
        if param.l4_protocol != IpProtocol::TCP {
            return Err(error::Error::InvalidIpProtocol);
        }

        let mut info = MysqlInfo::default();
        info.protocol_version = self.protocol_version;
        info.is_tls = param.is_tls();
        if self.perf_stats.is_none() && param.parse_perf {
            self.perf_stats = Some(L7PerfStats::default())
        };

        if self.has_compressed_header.is_none() {
            let _ = self.check_compressed_header(payload)?;
        }

        let mut decompress_buffer = take_buffer();
        let result = self.parse(
            param.parse_config,
            &mut decompress_buffer,
            payload,
            param.direction,
            &mut info,
        );
        give_buffer(decompress_buffer);
        match result {
            Ok(is_greeting) => {
                // ignore greeting
                if is_greeting {
                    return Ok(L7ParseResult::None);
                }
            }
            Err(Error::IgnoredPacket(header)) => {
                debug!("ignored packet with header: {header:?}");
                return Ok(L7ParseResult::None);
            }
            Err(Error::Truncated(_)) if param.direction == PacketDirection::ServerToClient => {
                // We are assuming large truncated or segmented responses to be `OK`
                // because `ERR` responses are likely to be short.
                info.msg_type = LogMessageType::Response;
                info.status = L7ResponseStatus::Ok;
            }
            Err(Error::Truncated(t)) => {
                debug!("truncated: {t:?} {param:?} {payload:?}");
                return Ok(L7ParseResult::None);
            }
            Err(Error::CompressedPacketNotParsed)
                if param.direction == PacketDirection::ServerToClient =>
            {
                info.msg_type = LogMessageType::Response;
                info.status = L7ResponseStatus::ParseFailed;
            }
            Err(Error::CommandNotSupported(c)) => {
                debug!("command not supported: {c}");
                return Ok(L7ParseResult::None);
            }
            Err(e) => return Err(e.into()),
        }

        set_captured_byte!(info, param);
        if let Some(config) = param.parse_config {
            info.set_is_on_blacklist(config);
        }
        if !info.is_on_blacklist && !self.last_is_on_blacklist {
            match param.direction {
                PacketDirection::ClientToServer => {
                    self.perf_stats.as_mut().map(|p| p.inc_req());
                }
                PacketDirection::ServerToClient => {
                    self.perf_stats.as_mut().map(|p| p.inc_resp());
                }
            }
            match info.status {
                L7ResponseStatus::ClientError => {
                    self.perf_stats
                        .as_mut()
                        .map(|p: &mut L7PerfStats| p.inc_req_err());
                }
                L7ResponseStatus::ServerError => {
                    self.perf_stats
                        .as_mut()
                        .map(|p: &mut L7PerfStats| p.inc_resp_err());
                }
                _ => {}
            }
            if info.msg_type == LogMessageType::Request || info.msg_type == LogMessageType::Response
            {
                info.cal_rrt(param, &None).map(|(rrt, _)| {
                    info.rrt = rrt;
                    self.perf_stats.as_mut().map(|p| p.update_rrt(rrt));
                });
            }
        }
        self.last_is_on_blacklist = info.is_on_blacklist;
        if param.parse_log {
            Ok(L7ParseResult::Single(L7ProtocolInfo::MysqlInfo(info)))
        } else {
            Ok(L7ParseResult::None)
        }
    }

    fn parsable_on_udp(&self) -> bool {
        false
    }

    fn protocol(&self) -> L7Protocol {
        L7Protocol::MySQL
    }

    fn perf_stats(&mut self) -> Option<L7PerfStats> {
        self.perf_stats.take()
    }

    fn set_obfuscate_cache(&mut self, obfuscate_cache: Option<ObfuscateCache>) {
        self.obfuscate_cache = obfuscate_cache;
    }
}

fn mysql_string(payload: &[u8]) -> &[u8] {
    if payload.len() > 2 && payload[0] == 0 && payload[1] == 1 {
        // MYSQL 8.0.26返回字符串前有0x0、0x1，MYSQL 8.0.21版本没有这个问题
        // https://gitlab.yunshan.net/platform/trident/-/merge_requests/2592#note_401425
        &payload[2..]
    } else {
        payload
    }
}

#[derive(PartialEq)]
enum SqlState {
    None,
    Equal,
    Less,
    Greater,
    In1,
    In2,
    In3,
    Values1,
    Values2,
    Values3,
    Values4,
    Values5,
    Values6,
    Values7,
    Like1,
    Like2,
    Like3,
    Like4,
    ValuesPause,
}

#[derive(Default)]
struct ParameterCounter(u32);

impl ParameterCounter {
    fn reset(&mut self) {
        self.0 = 0;
    }

    fn set(&mut self, sql: &[u8]) {
        let mut counter = 0;
        let mut state = SqlState::None;
        for byte in sql.iter() {
            match *byte {
                b'=' => state = SqlState::Equal,
                b'?' if state == SqlState::Equal => {
                    counter += 1;
                    state = SqlState::None;
                }
                b'>' if state == SqlState::None => state = SqlState::Greater,
                b'?' if state == SqlState::Greater => {
                    counter += 1;
                    state = SqlState::None;
                }
                _ if state == SqlState::Greater => state = SqlState::None,
                b'<' if state == SqlState::None => state = SqlState::Less,
                b'>' if state == SqlState::Less => state = SqlState::Greater,
                b'?' if state == SqlState::Less => {
                    counter += 1;
                    state = SqlState::None;
                }
                _ if state == SqlState::Less => state = SqlState::None,
                b'I' if state == SqlState::None => state = SqlState::In1,
                b'N' if state == SqlState::In1 => state = SqlState::In2,
                b'(' if state == SqlState::In2 => state = SqlState::In3,
                b',' if state == SqlState::In3 => {}
                b'?' if state == SqlState::In3 => counter += 1,
                b')' if state == SqlState::In3 => state = SqlState::None,
                b'V' if state == SqlState::None => state = SqlState::Values1,
                b'A' if state == SqlState::Values1 => state = SqlState::Values2,
                b'L' if state == SqlState::Values2 => state = SqlState::Values3,
                b'U' if state == SqlState::Values3 => state = SqlState::Values4,
                b'E' if state == SqlState::Values4 => state = SqlState::Values5,
                b'S' if state == SqlState::Values5 => state = SqlState::Values6,
                b' ' | b',' if state == SqlState::Values6 => {}
                b'(' if state == SqlState::Values6 => state = SqlState::Values7,
                b'?' if state == SqlState::Values7 => {
                    counter += 1;
                    state = SqlState::ValuesPause;
                }
                _ if state == SqlState::Values7 => {}
                b')' if state == SqlState::ValuesPause => state = SqlState::Values6,
                b'?' if state == SqlState::ValuesPause => {}
                b',' if state == SqlState::ValuesPause => state = SqlState::Values7,
                b'L' if state == SqlState::None => state = SqlState::Like1,
                b'I' if state == SqlState::Like1 => state = SqlState::Like2,
                b'K' if state == SqlState::Like2 => state = SqlState::Like3,
                b'E' if state == SqlState::Like3 => state = SqlState::Like4,
                b'?' if state == SqlState::Like4 => {
                    counter += 1;
                    state = SqlState::None;
                }
                b' ' => {}
                _ => state = SqlState::None,
            }
        }

        self.0 = counter;
    }

    fn get(&mut self, payload: &[u8], info: &mut MysqlInfo) {
        if self.0 == 0 {
            return;
        }
        let mut params = vec![];
        let mut offset = 0;
        // TODO: Only support first call or rebound.
        for byte in payload {
            offset += 1;
            if *byte == 0x01 {
                break;
            }
        }
        for _ in 0..self.0 as usize {
            if offset + PARAMETER_TYPE_LEN > payload.len() {
                return;
            }
            params.push((FieldType::from(payload[offset]), payload[offset + 1]));
            offset += PARAMETER_TYPE_LEN;
        }

        let mut context = String::new();
        for (i, (field_type, _)) in params.iter().enumerate() {
            if offset > payload.len() {
                break;
            }

            if let Some(length) = field_type.decode(&payload[offset..], &mut context) {
                if i != params.len() - 1 {
                    context.push_str(" , ");
                }
                offset += length;
            }
        }

        info.context = context;
    }
}

impl MysqlLog {
    fn check_compressed_header(&mut self, payload: &[u8]) -> Result<()> {
        self.has_compressed_header = Some(PayloadParser::is_compressed(payload)?);
        Ok(())
    }

    // because mysql packet sequence is wrapped above 255, seq 0 from server is not necessarily a greeting packet
    // use this to check if the packet is a greeting packet when sequence is 0
    fn is_greeting(mut payload: &[u8]) -> bool {
        // according to: https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_packets_protocol_handshake.html
        // the protocol version of a mysql packet can only be 0x09 or 0x0a
        match payload.get(PROTOCOL_VERSION_OFFSET) {
            Some(0x09 | 0x0a) => (),
            _ => return false,
        }

        // check server version
        payload = &payload[SERVER_VERSION_OFFSET..];
        // only EOF
        if payload.len() <= 1 {
            return false;
        }
        let Some(eos) = payload.iter().position(|&x| x == SERVER_VERSION_EOF) else {
            return false;
        };
        (&payload[..eos])
            .iter()
            .all(|x| *x == b'.' || x.is_ascii_digit())
    }

    fn greeting(payload: &[u8], info: &mut MysqlInfo) -> Result<u8> {
        let mut remain = payload.len();
        if remain < PROTOCOL_VERSION_LEN {
            return Err(Error::Truncated(TruncationType::Greeting));
        }
        let protocol_version = payload[PROTOCOL_VERSION_OFFSET];
        remain -= PROTOCOL_VERSION_LEN;
        let server_version_pos = payload[SERVER_VERSION_OFFSET..]
            .iter()
            .position(|&x| x == SERVER_VERSION_EOF)
            .unwrap_or_default();
        if server_version_pos <= 0 {
            return Err(Error::Truncated(TruncationType::Greeting));
        }
        remain -= server_version_pos as usize;
        if remain < THREAD_ID_LEN + 1 {
            return Err(Error::Truncated(TruncationType::Greeting));
        }
        info.status = L7ResponseStatus::Ok;
        Ok(protocol_version)
    }

    fn request(
        config: Option<&LogParserConfig>,
        payload: &[u8],
        pc: &mut ParameterCounter,
        info: &mut MysqlInfo,
        obfuscate_cache: &Option<ObfuscateCache>,
    ) -> Result<LogMessageType> {
        if payload.len() < COMMAND_LEN {
            return Err(Error::Truncated(TruncationType::Request));
        }
        info.command = payload[COMMAND_OFFSET];
        let mut msg_type = LogMessageType::Request;
        match info.command {
            COM_QUIT | COM_STMT_CLOSE => {
                msg_type = LogMessageType::Session;
                info.status = L7ResponseStatus::Ok;
            }
            COM_FIELD_LIST | COM_STMT_FETCH => (),
            COM_INIT_DB | COM_QUERY => {
                info.request_string(
                    config,
                    &payload[COMMAND_OFFSET + COMMAND_LEN..],
                    obfuscate_cache,
                )?;
            }
            COM_STMT_PREPARE => {
                info.request_string(
                    config,
                    &payload[COMMAND_OFFSET + COMMAND_LEN..],
                    obfuscate_cache,
                )?;
                if let Some(config) = config {
                    if config
                        .obfuscate_enabled_protocols
                        .is_disabled(L7Protocol::MySQL)
                    {
                        pc.set(info.context.as_bytes());
                    }
                }
            }
            COM_STMT_EXECUTE => {
                info.statement_id(&payload[STATEMENT_ID_OFFSET..]);
                if payload.len() > EXECUTE_STATEMENT_PARAMS_OFFSET {
                    pc.get(&payload[EXECUTE_STATEMENT_PARAMS_OFFSET..], info);
                }
                pc.reset();
            }
            COM_PING => {}
            _ => return Err(Error::CommandNotSupported(info.command)),
        }
        Ok(msg_type)
    }

    fn decode_compress_int(payload: &[u8]) -> u64 {
        let remain = payload.len();
        if remain == 0 {
            return 0;
        }
        let value = payload[0];
        match value {
            INT_FLAGS_2 if remain > INT_BASE_LEN + 2 => {
                bytes::read_u16_le(&payload[INT_BASE_LEN..]) as u64
            }
            INT_FLAGS_3 if remain > INT_BASE_LEN + 3 => {
                bytes::read_u16_le(&payload[INT_BASE_LEN..]) as u64
                    | ((payload[INT_BASE_LEN + 2] as u64) << 16)
            }
            INT_FLAGS_8 if remain > INT_BASE_LEN + 8 => {
                bytes::read_u64_le(&payload[INT_BASE_LEN..])
            }
            _ => value as u64,
        }
    }

    fn set_status(status_code: u16, info: &mut MysqlInfo) {
        if status_code != 0 {
            if status_code >= CLIENT_STATUS_CODE_MIN && status_code <= CLIENT_STATUS_CODE_MAX {
                info.status = L7ResponseStatus::ClientError;
            } else {
                info.status = L7ResponseStatus::ServerError;
            }
        } else {
            info.status = L7ResponseStatus::Ok;
        }
    }

    fn response(payload: &[u8], info: &mut MysqlInfo) -> Result<()> {
        let mut remain = payload.len();
        if remain < RESPONSE_CODE_LEN {
            return Err(Error::Truncated(TruncationType::Response));
        }
        info.response_code = payload[RESPONSE_CODE_OFFSET];
        remain -= RESPONSE_CODE_LEN;
        match info.response_code {
            MYSQL_RESPONSE_CODE_ERR => {
                if remain > ERROR_CODE_LEN {
                    let code = bytes::read_u16_le(&payload[ERROR_CODE_OFFSET..]);
                    if code < SERVER_STATUS_CODE_MIN || code > CLIENT_STATUS_CODE_MAX {
                        return Err(Error::InvalidResponseErrorCode(code));
                    }
                    info.error_code = Some(code as i32);
                    Self::set_status(code, info);
                    remain -= ERROR_CODE_LEN;
                }
                let error_message_offset =
                    if remain > SQL_STATE_LEN && payload[SQL_STATE_OFFSET] == SQL_STATE_MARKER {
                        SQL_STATE_OFFSET + SQL_STATE_LEN
                    } else {
                        SQL_STATE_OFFSET
                    };
                if error_message_offset < payload.len() {
                    let context = mysql_string(&payload[error_message_offset..]);
                    if !context.is_ascii() {
                        return Err(Error::InvalidResponseErrorMessage);
                    }
                    info.error_message = String::from_utf8_lossy(context).into_owned();
                }
            }
            MYSQL_RESPONSE_CODE_EOF => info.status = L7ResponseStatus::Ok,
            MYSQL_RESPONSE_CODE_OK => {
                info.status = L7ResponseStatus::Ok;
                info.affected_rows =
                    MysqlLog::decode_compress_int(&payload[AFFECTED_ROWS_OFFSET..]);
                info.statement_id(&payload[STATEMENT_ID_OFFSET..]);
            }
            _ => (),
        }
        Ok(())
    }

    fn string_null(payload: &[u8]) -> Option<&str> {
        let mut n = 0;
        for b in payload {
            if !b.is_ascii() {
                return None;
            }
            if *b == 0 {
                break;
            }
            n += 1;
        }

        if n == 0 {
            return None;
        }

        str::from_utf8(&payload[..n]).ok()
    }

    fn login(payload: &[u8], info: &mut MysqlInfo) -> Result<()> {
        if payload.len() < LOGIN_USERNAME_OFFSET {
            return Err(Error::Truncated(TruncationType::Login));
        }
        let client_capabilities_flags =
            bytes::read_u16_le(&payload[CLIENT_CAPABILITIES_FLAGS_OFFSET..]);
        if client_capabilities_flags & CLIENT_PROTOCOL_41 != CLIENT_PROTOCOL_41 {
            return Err(Error::InvalidLoginInfo(
                "unsupported client capabilities flags",
            ));
        }
        if !payload[FILTER_OFFSET..FILTER_OFFSET + FILTER_SIZE]
            .iter()
            .all(|b| *b == 0)
        {
            return Err(Error::InvalidLoginInfo("bad filter"));
        }

        match Self::string_null(&payload[LOGIN_USERNAME_OFFSET..]) {
            Some(context) if context.is_ascii() => {
                info.context = format!("Login username: {}", context);
            }
            _ => return Err(Error::InvalidLoginInfo("username not found or not ascii")),
        }

        info.status = L7ResponseStatus::Ok;

        Ok(())
    }

    fn is_interested_response(code: u8) -> bool {
        code == MYSQL_RESPONSE_CODE_OK
            || code == MYSQL_RESPONSE_CODE_ERR
            || code == MYSQL_RESPONSE_CODE_EOF
    }

    fn check(
        config: Option<&LogParserConfig>,
        decompress_buffer: &mut Vec<u8>,
        has_compressed_header: bool,
        payload: &[u8],
    ) -> bool {
        let decompress = config
            .map(|c| c.mysql_decompress_payload)
            .unwrap_or(LogParserConfig::default().mysql_decompress_payload);

        let mut parser = match PayloadParser::new(decompress, has_compressed_header, payload) {
            Ok(parser) => parser,
            Err(e) => {
                debug!("create payload parser failed: {e}");
                return false;
            }
        };

        let (header, payload) = match parser.try_next(decompress_buffer) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                debug!("no payload found in mysql packet");
                return false;
            }
            Err(e) => {
                debug!("parse mysql payload failed: {e}");
                return false;
            }
        };

        let Some(protocol_version_or_query_type) = payload.get(0) else {
            return false;
        };
        match *protocol_version_or_query_type {
            COM_QUERY | COM_STMT_PREPARE if header.seq_id == 0 => {
                let context = mysql_string(&payload[1..]);
                context.is_ascii() && is_mysql(context)
            }
            _ if header.seq_id != 0 => {
                let mut log_info = MysqlInfo::default();
                MysqlLog::login(payload, &mut log_info).is_ok()
            }
            _ => false,
        }
    }

    fn infer_message_type(
        direction: PacketDirection,
        header: &MysqlHeader,
        payload: &[u8],
    ) -> Option<LogMessageType> {
        if header.length == 0 {
            return None;
        }

        match direction {
            // greeting
            PacketDirection::ServerToClient if header.seq_id == 0 && Self::is_greeting(payload) => {
                if payload.len() < PROTOCOL_VERSION_LEN {
                    return None;
                }
                let protocol_version = payload[PROTOCOL_VERSION_OFFSET];
                let index = payload[SERVER_VERSION_OFFSET..]
                    .iter()
                    .position(|&x| x == SERVER_VERSION_EOF)?;
                if index != 0 && protocol_version == PROTOCOL_VERSION {
                    Some(LogMessageType::Other)
                } else {
                    None
                }
            }
            PacketDirection::ServerToClient => Some(LogMessageType::Response),
            PacketDirection::ClientToServer if header.seq_id <= 1 => Some(LogMessageType::Request),
            _ => None,
        }
    }

    // return is_greeting?
    fn parse(
        &mut self,
        config: Option<&LogParserConfig>,
        decompress_buffer: &mut Vec<u8>,
        payload: &[u8],
        direction: PacketDirection,
        info: &mut MysqlInfo,
    ) -> Result<bool> {
        let decompress = config
            .map(|c| c.mysql_decompress_payload)
            .unwrap_or(LogParserConfig::default().mysql_decompress_payload);

        let mut parser =
            PayloadParser::new(decompress, self.has_compressed_header.unwrap(), payload)?;
        // interested packets:
        // - the first packet in request
        // - greetings packet in response
        // - packet in response with response_code in [MYSQL_RESPONSE_CODE_OK, MYSQL_RESPONSE_CODE_ERR, MYSQL_RESPONSE_CODE_EOF]
        let (header, payload) = if direction == PacketDirection::ClientToServer {
            match parser.try_next(decompress_buffer)? {
                Some(frame) => frame,
                None => return Err(Error::NoPacket),
            }
        } else {
            loop {
                match parser.try_next(decompress_buffer)? {
                    Some((h, p)) => {
                        if h.length == 0 {
                            continue;
                        }
                        trace!("mysql response frame: {h:?} payload: {p:?}");
                        // greeting (seq == 0) or OK/EOF/ERR
                        if (h.seq_id == 0 && Self::is_greeting(p))
                            || p.get(RESPONSE_CODE_OFFSET)
                                .map(|c| Self::is_interested_response(*c))
                                .unwrap_or(false)
                        {
                            break (h, p);
                        }
                    }
                    None => return Err(Error::Truncated(TruncationType::Packet)),
                }
            }
        };

        let Some(mut msg_type) = Self::infer_message_type(direction, &header, payload) else {
            return Err(Error::IgnoredPacket(header));
        };
        match msg_type {
            LogMessageType::Request if header.seq_id == 0 => {
                msg_type =
                    Self::request(config, payload, &mut self.pc, info, &self.obfuscate_cache)?;
                if msg_type == LogMessageType::Request {
                    self.has_request = true;
                    self.has_login = false;
                }
            }
            LogMessageType::Request
                if direction == PacketDirection::ClientToServer && header.seq_id == 1 =>
            {
                Self::login(payload, info)?;
                self.has_login = true;
            }
            LogMessageType::Response
                if self.has_login && payload[RESPONSE_CODE_OFFSET] == MYSQL_RESPONSE_CODE_OK
                    || payload[RESPONSE_CODE_OFFSET] == MYSQL_RESPONSE_CODE_ERR =>
            {
                Self::response(payload, info)?;
                self.has_login = false;
            }
            LogMessageType::Response if self.has_request => {
                Self::response(payload, info)?;
                self.has_request = false;
            }
            LogMessageType::Other => {
                self.protocol_version = Self::greeting(payload, info)?;
                return Ok(true);
            }
            _ => return Err(Error::IgnoredPacket(header)),
        };
        info.msg_type = msg_type;

        Ok(false)
    }
}

// MySQL can have compressed payloads.
// If mysql decompress payload is enabled, agent will try to decompress the payload before parsing.
//
// ref: https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basic_compression.html
struct PayloadParser<'a> {
    payload: &'a [u8],
    decoder: Option<ZlibDecoder<&'a [u8]>>,
}

impl<'a> PayloadParser<'a> {
    // check if the payload has a compression header
    // only work if the payload is not truncated
    fn is_compressed(payload: &[u8]) -> Result<bool> {
        if payload.len() >= COMPRESS_HEADER_LEN {
            let compressed_len = (bytes::read_u32_le(&payload[..]) & 0xffffff) as usize;
            let uncompressed_len = bytes::read_u16_le(&payload[COMPRESS_HEADER_UNCOMPRESS_OFFSET..])
                as usize
                | (payload[COMPRESS_HEADER_UNCOMPRESS_OFFSET + 2] as usize) << 16;
            // there's only one compressed mysql packet in tcp payload
            if (uncompressed_len == 0 || uncompressed_len >= compressed_len)
                && COMPRESS_HEADER_LEN + compressed_len == payload.len()
            {
                return Ok(true);
            }
        }

        let mut offset = 0;
        while offset + HEADER_LEN < payload.len() {
            let header = MysqlHeader::new(&payload[offset..]);
            if offset + HEADER_LEN + header.length as usize > payload.len() {
                return Err(Error::Truncated(TruncationType::PacketPayload(header)));
            }
            offset += HEADER_LEN + header.length as usize;
            if offset == payload.len() {
                return Ok(false);
            }
        }
        Err(Error::Truncated(TruncationType::PacketHeader))
    }

    fn new(decompress: bool, has_compressed_header: bool, payload: &'a [u8]) -> Result<Self> {
        let compressed = if has_compressed_header {
            if payload.len() < COMPRESS_HEADER_LEN {
                return Err(Error::Truncated(TruncationType::CompressedHeader));
            }
            let uncompressed_len = bytes::read_u16_le(&payload[COMPRESS_HEADER_UNCOMPRESS_OFFSET..])
                as usize
                | (payload[COMPRESS_HEADER_UNCOMPRESS_OFFSET + 2] as usize) << 16;
            // if uncompressed_len is 0, it means the payload is not compressed
            // otherwise, it's compressed
            uncompressed_len != 0
        } else {
            false
        };

        if compressed {
            if !decompress {
                Err(Error::CompressedPacketNotParsed)
            } else {
                Ok(Self {
                    payload,
                    decoder: Some(ZlibDecoder::new(&payload[COMPRESS_HEADER_LEN..])),
                })
            }
        } else {
            Ok(Self {
                payload: if has_compressed_header {
                    &payload[COMPRESS_HEADER_LEN..]
                } else {
                    payload
                },
                decoder: None,
            })
        }
    }

    fn try_next<'b, 'c>(
        &mut self,
        buffer: &'b mut Vec<u8>,
    ) -> Result<Option<(MysqlHeader, &'c [u8])>>
    where
        'a: 'c,
        'b: 'c,
    {
        match self.decoder.as_mut() {
            Some(decoder) => {
                // mysql compression is a layer above mysql packet
                // it's not aware of mysql packet boundaries
                // so decompressed packet can have a part of uncompressed mysql packet
                // reading header or payload can fail in this situation
                let mut hb = [0u8; HEADER_LEN];
                if let Err(_) = decoder.read_exact(&mut hb) {
                    return Err(Error::Truncated(TruncationType::CompressedPacketHeader));
                }
                let header = MysqlHeader::new(&hb);
                Self::fill_buffer(decoder, buffer, header.length as usize);
                Ok(Some((header, &buffer[..])))
            }
            None => {
                if self.payload.len() < HEADER_LEN {
                    self.payload = &self.payload[self.payload.len()..];
                    return Err(Error::Truncated(TruncationType::PacketHeader));
                }
                let header = MysqlHeader::new(&self.payload[..HEADER_LEN]);
                let end_of_frame = self.payload.len().min(HEADER_LEN + header.length as usize);
                let frame = &self.payload[HEADER_LEN..end_of_frame];
                self.payload = &self.payload[end_of_frame..];
                Ok(Some((header, frame)))
            }
        }
    }

    // do not use read_exact because on failure, it will consume decoder data without putting successful reads into buffer
    fn fill_buffer<R: Read>(mut decoder: R, buffer: &mut Vec<u8>, length: usize) {
        buffer.clear();
        buffer.resize(length, 0);

        let mut offset = 0;
        loop {
            match decoder.read(&mut buffer[offset..]) {
                Ok(n) if n == 0 => break,
                Ok(n) if n + offset == length => return,
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }

        buffer.resize(offset, 0);
    }
}

#[derive(Debug, Default)]
pub struct MysqlHeader {
    length: u32,
    seq_id: u8,
}

impl MysqlHeader {
    pub fn new(payload: &[u8]) -> Self {
        assert!(payload.len() >= HEADER_LEN);
        let bytes_as_u32 = bytes::read_u32_le(&payload[..]);
        Self {
            length: bytes_as_u32 & 0xffffff,
            seq_id: (bytes_as_u32 >> 24) as u8,
        }
    }
}

#[derive(PartialEq)]
enum Token {
    Key,
    Separator,
    Value,
}

pub struct KvExtractor<'a> {
    split: Box<dyn Iterator<Item = &'a str> + 'a>,
    last_segment: Option<&'a str>,
}

impl<'a> KvExtractor<'a> {
    pub fn new(s: &'a str) -> Self {
        Self {
            split: Box::new(
                s.split_inclusive(|c: char| {
                    c.is_ascii_whitespace() || c == ':' || c == '=' || c == ','
                })
                .into_iter(),
            ),
            last_segment: None,
        }
    }
}

impl<'a> Iterator for KvExtractor<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let mut next_token = Token::Key;
        let mut last_key = None;
        loop {
            let Some(seg) = self
                .last_segment
                .take()
                .or_else(|| self.split.as_mut().next())
            else {
                return None;
            };

            let Some((last, sep_char)) = seg.char_indices().last() else {
                continue;
            };
            let (mut exp, sep) = seg.split_at(last);
            match sep {
                "," => {
                    if !exp.is_empty() && next_token == Token::Value {
                        return Some((last_key.unwrap(), exp.trim()));
                    }
                    next_token = Token::Key; // resets
                }
                ":" | "=" => {
                    if !exp.is_empty() {
                        if next_token == Token::Value {
                            self.last_segment.replace(seg);
                            return Some((last_key.unwrap(), exp.trim()));
                        }
                        // discard previous parsed key if any
                        last_key = Some(exp.trim());
                        next_token = Token::Value;
                    } else if next_token == Token::Separator {
                        assert!(last_key.is_some());
                        next_token = Token::Value;
                    } else {
                        // invalid separator
                        next_token = Token::Key;
                    }
                }
                _ => {
                    if exp.is_empty() {
                        continue;
                    }
                    if !sep_char.is_ascii_whitespace() {
                        exp = seg;
                    }
                    match next_token {
                        Token::Key | Token::Separator => {
                            last_key = Some(exp.trim());
                            next_token = Token::Separator;
                        }
                        Token::Value => {
                            self.last_segment.replace(seg);
                            return Some((last_key.unwrap(), exp.trim()));
                        }
                    }
                }
            }
        }
    }
}

// test log parse
#[cfg(test)]
mod tests {
    use std::fmt::Write;
    use std::path::Path;
    use std::rc::Rc;
    use std::{cell::RefCell, fs};

    use super::*;

    use crate::{
        common::{flow::PacketDirection, l7_protocol_log::L7PerfCache, MetaPacket},
        config::{handler::TraceType, ExtraLogFields},
        flow_generator::L7_RRT_CACHE_CAPACITY,
        utils::test::Capture,
    };

    const FILE_DIR: &str = "resources/test/flow_generator/mysql";

    fn run(name: &str, truncate: Option<usize>) -> String {
        let pcap_file = Path::new(FILE_DIR).join(name);
        let capture = Capture::load_pcap(pcap_file);
        let log_cache = Rc::new(RefCell::new(L7PerfCache::new(L7_RRT_CACHE_CAPACITY)));
        let mut packets = capture.collect::<Vec<_>>();
        if packets.is_empty() {
            return "".to_string();
        }

        let mut mysql = MysqlLog::default();
        let mut output: String = String::new();
        let first_dst_port = packets[0].lookup_key.dst_port;
        let mut previous_command = 0u8;
        let log_config = LogParserConfig::default();
        for packet in packets.iter_mut() {
            packet.lookup_key.direction = if packet.lookup_key.dst_port == first_dst_port {
                PacketDirection::ClientToServer
            } else {
                PacketDirection::ServerToClient
            };
            let payload = match packet.get_l4_payload() {
                Some(p) => match truncate {
                    Some(t) if t < p.len() => &p[..t],
                    _ => p,
                },
                None => continue,
            };
            let is_mysql = mysql.check_payload(
                payload,
                &ParseParam::new(
                    packet as &MetaPacket,
                    log_cache.clone(),
                    Default::default(),
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    Default::default(),
                    true,
                    true,
                ),
            );

            let mut param = ParseParam::new(
                &*packet,
                log_cache.clone(),
                Default::default(),
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Default::default(),
                true,
                true,
            );
            param.parse_config = Some(&log_config);
            param.set_captured_byte(payload.len());

            let info = mysql.parse_payload(payload, &param);

            if let Ok(info) = info {
                if info.is_none() {
                    let mut i = MysqlInfo::default();
                    i.protocol_version = mysql.protocol_version;
                    let _ = write!(
                        &mut output,
                        "{} is_mysql: {}\n",
                        serde_json::to_string(&i).unwrap(),
                        is_mysql
                    );
                    previous_command = 0;
                    continue;
                }
                match info.unwrap_single() {
                    L7ProtocolInfo::MysqlInfo(mut i) => {
                        if i.app_proto_head().unwrap().msg_type == LogMessageType::Request {
                            previous_command = i.command;
                        } else {
                            if previous_command != COM_QUERY {
                                i.affected_rows = 0;
                            }
                            previous_command = 0;
                        }

                        i.rrt = 0;
                        let _ = write!(
                            &mut output,
                            "{} is_mysql: {}\n",
                            serde_json::to_string(&i).unwrap(),
                            is_mysql
                        );
                    }
                    _ => unreachable!(),
                }
            } else {
                let mut i = MysqlInfo::default();
                i.protocol_version = mysql.protocol_version;
                let _ = write!(
                    &mut output,
                    "{} is_mysql: {}\n",
                    serde_json::to_string(&i).unwrap(),
                    is_mysql
                );
            }
        }
        output
    }

    #[test]
    fn check() {
        let files = vec![
            ("mysql-use.pcap", "mysql-use.result"),
            ("mysql-exec.pcap", "mysql-exec.result"),
            ("mysql-statement-id.pcap", "mysql-statement-id.result"),
            ("mysql-statement.pcap", "mysql-statement.result"),
            ("mysql.pcap", "mysql.result"),
            ("mysql-error.pcap", "mysql-error.result"),
            ("mysql-table-desc.pcap", "mysql-table-desc.result"),
            ("mysql-table-insert.pcap", "mysql-table-insert.result"),
            ("mysql-table-delete.pcap", "mysql-table-delete.result"),
            ("mysql-table-update.pcap", "mysql-table-update.result"),
            ("mysql-table-select.pcap", "mysql-table-select.result"),
            ("mysql-table-create.pcap", "mysql-table-create.result"),
            ("mysql-table-destroy.pcap", "mysql-table-destroy.result"),
            ("mysql-table-alter.pcap", "mysql-table-alter.result"),
            ("mysql-database.pcap", "mysql-database.result"),
            ("mysql-login-error.pcap", "mysql-login-error.result"),
            (
                "mysql-compressed-response.pcap",
                "mysql-compressed-response.result",
            ),
            (
                "partial-packet-compressed.pcap",
                "partial-packet-compressed.result",
            ),
        ];

        for item in files.iter() {
            let expected = fs::read_to_string(&Path::new(FILE_DIR).join(item.1)).unwrap();
            let output = run(item.0, None);

            if output != expected {
                let output_path = Path::new("actual.txt");
                fs::write(&output_path, &output).unwrap();
                assert!(
                    output == expected,
                    "output different from expected {}, written to {:?}",
                    item.1,
                    output_path
                );
            }
        }
    }

    #[test]
    fn check_perf() {
        let expecteds = vec![
            (
                "mysql.pcap",
                L7PerfStats {
                    request_count: 7,
                    response_count: 6,
                    err_client_count: 0,
                    err_server_count: 0,
                    err_timeout: 0,
                    rrt_count: 6,
                    rrt_sum: 598,
                    rrt_max: 225,
                    ..Default::default()
                },
            ),
            (
                "mysql-error.pcap",
                L7PerfStats {
                    request_count: 5,
                    response_count: 4,
                    err_client_count: 0,
                    err_server_count: 1,
                    err_timeout: 0,
                    rrt_count: 4,
                    rrt_sum: 292,
                    rrt_max: 146,
                    ..Default::default()
                },
            ),
            (
                "171-mysql.pcap",
                L7PerfStats {
                    request_count: 390,
                    response_count: 390,
                    err_client_count: 0,
                    err_server_count: 0,
                    err_timeout: 0,
                    rrt_count: 390,
                    rrt_sum: 127090,
                    rrt_max: 5355,
                    ..Default::default()
                },
            ),
        ];

        for item in expecteds.iter() {
            assert_eq!(item.1, run_perf(item.0), "pcap {} check failed", item.0);
        }
    }

    fn run_perf(pcap: &str) -> L7PerfStats {
        let rrt_cache = Rc::new(RefCell::new(L7PerfCache::new(100)));
        let mut mysql = MysqlLog::default();

        let capture = Capture::load_pcap(Path::new(FILE_DIR).join(pcap));
        let mut packets = capture.collect::<Vec<_>>();

        let first_src_mac = packets[0].lookup_key.src_mac;
        for packet in packets.iter_mut() {
            if packet.lookup_key.src_mac == first_src_mac {
                packet.lookup_key.direction = PacketDirection::ClientToServer;
            } else {
                packet.lookup_key.direction = PacketDirection::ServerToClient;
            }
            if packet.get_l4_payload().is_some() {
                let param = &ParseParam::new(
                    &*packet,
                    rrt_cache.clone(),
                    Default::default(),
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    Default::default(),
                    true,
                    true,
                );
                let _ = mysql.parse_payload(packet.get_l4_payload().unwrap(), param);
            }
        }
        mysql.perf_stats.unwrap()
    }

    #[test]
    fn check_truncate() {
        let files = vec![
            ("truncate-1024.pcap", "truncate-1024.result"),
            ("large-response.pcap", "large-response.result"),
        ];

        for item in files.iter() {
            let expected = fs::read_to_string(&Path::new(FILE_DIR).join(item.1)).unwrap();
            let output = run(item.0, Some(1024));

            if output != expected {
                let output_path = Path::new("actual.txt");
                fs::write(&output_path, &output).unwrap();
                assert!(
                    output == expected,
                    "output different from expected {}, written to {:?}",
                    item.1,
                    output_path
                );
            }
        }
    }

    #[test]
    fn comment_extractor() {
        flexi_logger::Logger::try_with_env()
            .unwrap()
            .start()
            .unwrap();
        let testcases = vec![
            (
                "/* traceparent: 00-trace_id-span_id-01 */ SELECT * FROM table",
                Some("trace_id"),
                Some("span_id"),
            ),
            (
                "/* traceparent: traceparent   \t : 00-trace_id-span_id-01 */ SELECT * FROM table",
                Some("trace_id"),
                Some("span_id"),
            ),
            (
                " SELECT * FROM table # traceparent: traceparent   \ttRaCeId \t: 00-trace_id-span_id-01: traceparent",
                Some("00-trace_id-span_id-01"),
                None,
            ),
            (
                "/* trcod=VCCMOYF2,svccod=VCCMOF2,jrnno=W557426527, reqseq=124748979092341,chanl=MB,userId=12094710GSOE */ SELECT * FROM table",
                Some("W557426527"),
                None,
            ),
        ];
        let mut info = MysqlInfo::default();
        let config = L7LogDynamicConfig::new(
            vec![],
            vec![],
            vec![
                TraceType::TraceParent,
                TraceType::Customize("TraceID".to_owned()),
                TraceType::Customize("jrnno".to_owned()),
            ],
            vec![TraceType::TraceParent],
            ExtraLogFields::default(),
            false,
            #[cfg(feature = "enterprise")]
            std::collections::HashMap::new(),
        );
        for (input, tid, sid) in testcases {
            info.trace_id = None;
            info.span_id = None;
            info.extract_trace_and_span_id(&config, input);
            assert_eq!(info.trace_id.as_ref().map(|s| s.as_str()), tid);
            assert_eq!(info.span_id.as_ref().map(|s| s.as_str()), sid);
        }
    }

    #[test]
    fn test_set_parameter_counter() {
        let cases =
            vec![
            ("=?", 1),
            ("= ?", 1),
            ("<> ?", 0),
            ("<>?", 1),
            ("< ?", 0),
            (">?", 1),
            ("<?", 1),
            ("IN (?) ?", 1),
            ("IN (?,?,?)", 3),
            ("VALUES (?,?,?,?,?,??),(?,?,?,?,?,?,?)", 13),
            ("VALUES (?,?,?,?,?,?,?)", 7),
            ("VALUES (?,?,?,DEFAULT,?,?,?,?)", 7),
            ("VALUES (?,?,?),(DEFAULT,?,?,?,?)", 7),
            (
                "SELECT * FROM ` ? ` WHERE ` ? `=? ? BY ` ? `.` ? ` LIMIT ?",
                1,
            ),
            (
                "SELECT ` ? `,` ? `,` ? ` FROM ` ? ` WHERE (namespace =?) AND (` ? ` LIKE ?)",
                2,
            ),
            ("SELECT ` ? ` FROM ` ? ` WHERE domain =? AND content <> ?  BY ` ? `.` ? ` LIMIT ?", 1),
        ];
        for case in cases {
            let mut pc = ParameterCounter::default();
            pc.set(case.0.as_bytes());
            assert_eq!(pc.0, case.1, "Cases {:?} error, actual is {}", case, pc.0);
        }
    }

    #[test]
    fn test_parse_parameter() {
        fn parse_parameter(field_type: FieldType, payload: Vec<u8>) -> String {
            let mut output = String::new();
            field_type.decode(&payload, &mut output);
            output
        }

        let mut cases = vec![
            (FieldType::Long, vec![1, 0, 0, 0], "Long(1)"),
            (FieldType::Int24, vec![1, 0, 0, 0], "Int24(1)"),
            (FieldType::Short, vec![1, 0], "Short(1)"),
            (FieldType::Year, vec![1, 0], "Years(1)"),
            (FieldType::Tiny, vec![1], "Tiny(1)"),
            (
                FieldType::Double,
                vec![0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x24, 0x40],
                "Double(10.2)",
            ),
            (
                FieldType::Float,
                vec![0x33, 0x33, 0x23, 0x41],
                "Float(10.2)",
            ),
            (
                FieldType::Date,
                vec![
                    0x0b, 0xda, 0x07, 0x0a, 0x11, 0x13, 0x1b, 0x1e, 0x01, 00, 00, 00,
                ],
                "datetime 2010-10-17 19:27:30.000001",
            ),
            (
                FieldType::Datetime,
                vec![0x04, 0xda, 0x07, 0x0a, 0x11],
                "datetime 2010-10-17",
            ),
            (
                FieldType::Timestamp,
                vec![
                    0x0b, 0xda, 0x07, 0x0a, 0x11, 0x13, 0x1b, 0x1e, 0x01, 00, 00, 00,
                ],
                "datetime 2010-10-17 19:27:30.000001",
            ),
            (
                FieldType::Time,
                vec![
                    0x0c, 0x01, 0x78, 0x00, 0x00, 0x00, 0x13, 0x1b, 0x1e, 0x01, 0x00, 0x00, 0x00,
                ],
                "time -120d 19:27:30.000001",
            ),
            (
                FieldType::Time,
                vec![0x08, 0x01, 0x78, 0x00, 0x00, 0x00, 0x13, 0x1b, 0x1e],
                "time -120d 19:27:30",
            ),
            (FieldType::Time, vec![0x1], "time 0d 00:00:00.000000"),
        ];

        for (i, (field_type, payload, except)) in cases.drain(..).enumerate() {
            let actual = parse_parameter(field_type, payload);
            assert_eq!(
                actual,
                except.to_string(),
                "Cases {:3} field type {:?} error: except: {} but actual: {}.",
                i + 1,
                field_type,
                except,
                actual
            );
        }
    }

    #[test]
    fn test_kv_extractor() {
        let cases = vec![
            ("safwa: asfew saefa:weaiow dff=ea,dsas=sad , asda=2,: ,a23 =zsda fawa:1,ara :af::, 2e=c:g,",
            vec![
                ("safwa", "asfew"),
                ("saefa", "weaiow"),
                ("dff", "ea"),
                ("dsas", "sad"),
                ("asda", "2"),
                ("a23", "zsda"),
                ("fawa", "1"),
                ("ara", "af"),
                ("2e", "c"),
                ("c", "g"),
            ]),
            (
                "traceparent: 00-trace_id-span_id-01",
                vec![("traceparent", "00-trace_id-span_id-01")]
                ),
            (
                "traceparent: traceparent   \t : 00-trace_id-span_id-01",
                vec![
                ("traceparent", "traceparent"),
                ("traceparent", "00-trace_id-span_id-01"),
                ]
            ),
            (
                " traceparent: traceparent   \ttRaCeId \t: 00-trace_id-span_id-01: traceparent",
                vec![
                ("traceparent", "traceparent"),
                ("tRaCeId", "00-trace_id-span_id-01"),
                ("00-trace_id-span_id-01", "traceparent"),
                ]
            ),
            (
                " trcod=VCCMOYF2,svccod=VCCMOF2,jrnno=W557426527, reqseq=124748979092341,chanl=MB,userId=12094710GSOE",
                vec![
                ("trcod", "VCCMOYF2"),
                ("svccod", "VCCMOF2"),
                ("jrnno", "W557426527"),
                ("reqseq", "124748979092341"),
                ("chanl", "MB"),
                ("userId", "12094710GSOE"),
                ]
            ),
        ];
        for (input, output) in cases {
            assert_eq!(
                output,
                KvExtractor::new(input).collect::<Vec<_>>(),
                "failed in case {input}",
            );
        }
    }
}
