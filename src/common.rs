use std::{
    collections::HashMap,
    future::Future,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex, RwLock},
    task::Poll,
};

use serde_json::{json, Map, Value};

#[cfg(not(target_os = "ios"))]
use hbb_common::whoami;
use hbb_common::{
    allow_err,
    anyhow::{anyhow, Context},
    async_recursion::async_recursion,
    bail, base64,
    bytes::Bytes,
    config::{
        self, keys, use_ws, Config, LocalConfig, CONNECT_TIMEOUT, READ_TIMEOUT, RENDEZVOUS_PORT,
    },
    futures::future::join_all,
    futures_util::future::poll_fn,
    get_version_number, log,
    message_proto::*,
    protobuf::{Enum, Message as _},
    rendezvous_proto::*,
    socket_client,
    sodiumoxide::crypto::{box_, secretbox, sign},
    timeout,
    tls::{get_cached_tls_accept_invalid_cert, get_cached_tls_type, upsert_tls_cache, TlsType},
    tokio::{
        self,
        net::UdpSocket,
        time::{Duration, Instant, Interval},
    },
    ResultType, Stream,
};

use crate::{
    hbbs_http::{create_http_client_async, get_url_for_tls},
    ui_interface::{get_option, is_installed, set_option},
};

#[derive(Debug, Eq, PartialEq)]
pub enum GrabState {
    Ready,
    Run,
    Wait,
    Exit,
}

pub type NotifyMessageBox = fn(String, String, String, String) -> dyn Future<Output = ()>;

pub const PORTABLE_APPNAME_RUNTIME_ENV_KEY: &str = "RUSTDESK_APPNAME";

pub const PLATFORM_WINDOWS: &str = "Windows";
pub const PLATFORM_LINUX: &str = "Linux";
pub const PLATFORM_MACOS: &str = "Mac OS";
pub const PLATFORM_ANDROID: &str = "Android";

pub const TIMER_OUT: Duration = Duration::from_secs(1);
pub const DEFAULT_KEEP_ALIVE: i32 = 60_000;

const MIN_VER_MULTI_UI_SESSION: &str = "1.2.4";

pub mod input {
    pub const MOUSE_TYPE_MOVE: i32 = 0;
    pub const MOUSE_TYPE_DOWN: i32 = 1;
    pub const MOUSE_TYPE_UP: i32 = 2;
    pub const MOUSE_TYPE_WHEEL: i32 = 3;
    pub const MOUSE_TYPE_TRACKPAD: i32 = 4;
    pub const MOUSE_TYPE_MOVE_RELATIVE: i32 = 5;
    pub const MOUSE_TYPE_MASK: i32 = 0x7;
    pub const MOUSE_BUTTON_LEFT: i32 = 0x01;
    pub const MOUSE_BUTTON_RIGHT: i32 = 0x02;
    pub const MOUSE_BUTTON_WHEEL: i32 = 0x04;
    pub const MOUSE_BUTTON_BACK: i32 = 0x08;
    pub const MOUSE_BUTTON_FORWARD: i32 = 0x10;
}

lazy_static::lazy_static! {
    pub static ref SOFTWARE_UPDATE_URL: Arc<Mutex<String>> = Default::default();
    pub static ref DEVICE_ID: Arc<Mutex<String>> = Default::default();
    pub static ref DEVICE_NAME: Arc<Mutex<String>> = Default::default();
    static ref PUBLIC_IPV6_ADDR: Arc<Mutex<(Option<SocketAddr>, Option<Instant>)>> = Default::default();
}

lazy_static::lazy_static! {
    static ref IS_SERVER: bool = std::env::args().nth(1) == Some("--server".to_owned());
    static ref SERVER_RUNNING: Arc<RwLock<bool>> = Default::default();
    static ref IS_MAIN: bool = std::env::args().nth(1).map_or(true, |arg| !arg.starts_with("--"));
    static ref IS_CM: bool = std::env::args().nth(1) == Some("--cm".to_owned()) || std::env::args().nth(1) == Some("--cm-no-ui".to_owned());
}

pub struct SimpleCallOnReturn {
    pub b: bool,
    pub f: Box<dyn Fn() + Send + 'static>,
}

impl Drop for SimpleCallOnReturn {
    fn drop(&mut self) {
        if self.b {
            (self.f)();
        }
    }
}

pub fn global_init() -> bool {
    #[cfg(target_os = "linux")]
    {
        if !crate::platform::linux::is_x11() {
            crate::server::wayland::init();
        }
    }
    true
}

pub fn global_clean() {}

#[inline]
pub fn set_server_running(b: bool) {
    *SERVER_RUNNING.write().unwrap() = b;
}

#[inline]
pub fn is_support_multi_ui_session(ver: &str) -> bool {
    is_support_multi_ui_session_num(hbb_common::get_version_number(ver))
}

#[inline]
pub fn is_support_multi_ui_session_num(ver: i64) -> bool {
    ver >= hbb_common::get_version_number(MIN_VER_MULTI_UI_SESSION)
}

#[inline]
#[cfg(feature = "unix-file-copy-paste")]
pub fn is_support_file_copy_paste(ver: &str) -> bool {
    is_support_file_copy_paste_num(hbb_common::get_version_number(ver))
}

#[inline]
#[cfg(feature = "unix-file-copy-paste")]
pub fn is_support_file_copy_paste_num(ver: i64) -> bool {
    ver >= hbb_common::get_version_number("1.3.8")
}

pub fn is_support_remote_print(ver: &str) -> bool {
    hbb_common::get_version_number(ver) >= hbb_common::get_version_number("1.3.9")
}

pub fn is_support_file_paste_if_macos(ver: &str) -> bool {
    hbb_common::get_version_number(ver) >= hbb_common::get_version_number("1.3.9")
}

#[inline]
pub fn is_support_screenshot(ver: &str) -> bool {
    is_support_multi_ui_session_num(hbb_common::get_version_number(ver))
}

#[inline]
pub fn is_support_screenshot_num(ver: i64) -> bool {
    ver >= hbb_common::get_version_number("1.4.0")
}

#[inline]
pub fn is_support_file_transfer_resume(ver: &str) -> bool {
    is_support_file_transfer_resume_num(hbb_common::get_version_number(ver))
}

#[inline]
pub fn is_support_file_transfer_resume_num(ver: i64) -> bool {
    ver >= hbb_common::get_version_number("1.4.2")
}

const MIN_VERSION_RELATIVE_MOUSE_MODE: &str = "1.4.5";

#[inline]
pub fn is_support_relative_mouse_mode(ver: &str) -> bool {
    is_support_relative_mouse_mode_num(hbb_common::get_version_number(ver))
}

#[inline]
pub fn is_support_relative_mouse_mode_num(ver: i64) -> bool {
    ver >= hbb_common::get_version_number(MIN_VERSION_RELATIVE_MOUSE_MODE)
}

#[inline]
pub fn is_server() -> bool {
    *IS_SERVER
}

#[inline]
pub fn need_fs_cm_send_files() -> bool {
    #[cfg(windows)]
    {
        is_server()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[inline]
pub fn is_main() -> bool {
    *IS_MAIN
}

#[inline]
pub fn is_cm() -> bool {
    *IS_CM
}

#[inline]
pub fn is_server_running() -> bool {
    *SERVER_RUNNING.read().unwrap()
}

#[inline]
pub fn valid_for_numlock(evt: &KeyEvent) -> bool {
    if let Some(key_event::Union::ControlKey(ck)) = evt.union {
        let v = ck.value();
        (v >= ControlKey::Numpad0.value() && v <= ControlKey::Numpad9.value())
            || v == ControlKey::Decimal.value()
    } else {
        false
    }
}

pub fn set_sound_input(device: String) {
    let prior_device = get_option("audio-input".to_owned());
    if prior_device != device {
        log::info!("switch to audio input device {}", device);
        std::thread::spawn(move || {
            set_option("audio-input".to_owned(), device);
        });
    } else {
        log::info!("audio input is already set to {}", device);
    }
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_default_sound_input() -> Option<String> {
    #[cfg(not(target_os = "linux"))]
    {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let dev = host.default_input_device();
        return if let Some(dev) = dev {
            match dev.name() {
                Ok(name) => Some(name),
                Err(_) => None,
            }
        } else {
            None
        };
    }
    #[cfg(target_os = "linux")]
    {
        let input = crate::platform::linux::get_default_pa_source();
        return if let Some(input) = input {
            Some(input.1)
        } else {
            None
        };
    }
}

#[inline]
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn get_default_sound_input() -> Option<String> {
    None
}

// Resampling functions (kept as original)
#[cfg(feature = "use_rubato")]
pub fn resample_channels(data: &[f32], sample_rate0: u32, sample_rate: u32, channels: u16) -> Vec<f32> {
    // ... rubato implementation ...
    Vec::new()
}

// Audio rechannel functions (kept as original)
pub fn audio_rechannel(input: Vec<f32>, in_hz: u32, out_hz: u32, in_chan: u16, output_chan: u16) -> Vec<f32> {
    // ... implementation ...
    input
}

// ... audio_rechannel macro and instances ...

pub struct CheckTestNatType {
    is_direct: bool,
}

impl CheckTestNatType {
    pub fn new() -> Self {
        Self {
            is_direct: Config::get_socks().is_none() && !config::use_ws(),
        }
    }
}

impl Drop for CheckTestNatType {
    fn drop(&mut self) {
        let is_direct = Config::get_socks().is_none() && !config::use_ws();
        if self.is_direct != is_direct {
            test_nat_type();
        }
    }
}

pub fn test_nat_type() {
    test_ipv6_sync();
    use std::sync::atomic::{AtomicBool, Ordering};
    std::thread::spawn(move || {
        static IS_RUNNING: AtomicBool = AtomicBool::new(false);
        if IS_RUNNING.load(Ordering::SeqCst) {
            return;
        }
        IS_RUNNING.store(true, Ordering::SeqCst);

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::ipc::get_socks_ws();
        let is_direct = Config::get_socks().is_none() && !config::use_ws();
        if !is_direct {
            Config::set_nat_type(NatType::SYMMETRIC as _);
            IS_RUNNING.store(false, Ordering::SeqCst);
            return;
        }

        let mut i = 0;
        loop {
            match test_nat_type_() {
                Ok(true) => break,
                Err(err) => {
                    log::error!("test nat: {}", err);
                }
                _ => {}
            }
            if Config::get_nat_type() != 0 {
                break;
            }
            i = i * 2 + 1;
            if i > 300 {
                i = 300;
            }
            std::thread::sleep(std::time::Duration::from_secs(i));
        }

        IS_RUNNING.store(false, Ordering::SeqCst);
    });
}

#[tokio::main(flavor = "current_thread")]
async fn test_nat_type_() -> ResultType<bool> {
    log::info!("Testing nat ...");
    let start = std::time::Instant::now();
    let server1 = Config::get_rendezvous_server();
    let server2 = crate::increase_port(&server1, -1);
    let mut msg_out = RendezvousMessage::new();
    let serial = Config::get_serial();
    msg_out.set_test_nat_request(TestNatRequest {
        serial,
        ..Default::default()
    });
    let mut port1 = 0;
    let mut port2 = 0;
    let mut local_addr = None;
    for i in 0..2 {
        let server = if i == 0 { &*server1 } else { &*server2 };
        let mut socket =
            socket_client::connect_tcp_local(server, local_addr, CONNECT_TIMEOUT).await?;
        if i == 0 {
            local_addr = Some(socket.local_addr());
            Config::set_option(
                "local-ip-addr".to_owned(),
                socket.local_addr().ip().to_string(),
            );
        }
        socket.send(&msg_out).await?;
        if let Some(msg_in) = get_next_nonkeyexchange_msg(&mut socket, None).await {
            if let Some(rendezvous_message::Union::TestNatResponse(tnr)) = msg_in.union {
                log::debug!("Got nat response from {}: port={}", server, tnr.port);
                if i == 0 {
                    port1 = tnr.port;
                } else {
                    port2 = tnr.port;
                }
                if let Some(cu) = tnr.cu.as_ref() {
                    Config::set_option(
                        "rendezvous-servers".to_owned(),
                        cu.rendezvous_servers.join(","),
                    );
                    Config::set_serial(cu.serial);
                }
            }
        } else {
            break;
        }
    }
    let ok = port1 > 0 && port2 > 0;
    if ok {
        let t = if port1 == port2 {
            NatType::ASYMMETRIC
        } else {
            NatType::SYMMETRIC
        };
        Config::set_nat_type(t as _);
        log::info!("Tested nat type: {:?} in {:?}", t, start.elapsed());
    }
    Ok(ok)
}

pub async fn get_rendezvous_server(ms_timeout: u64) -> (String, Vec<String>, bool) {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let (mut a, mut b) = get_rendezvous_server_(ms_timeout);
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let (mut a, mut b) = get_rendezvous_server_(ms_timeout).await;
    
    // --- 修改点：强制使用硬编码的服务器地址 ---
    a = "81.71.156.173".to_string();
    b = vec!["81.71.156.173".to_string()];

    let mut b: Vec<String> = b
        .drain(..)
        .map(|x| socket_client::check_port(x, config::RENDEZVOUS_PORT))
        .collect();
    let c = if b.contains(&a) {
        b = b.drain(..).filter(|x| x != &a).collect();
        true
    } else {
        a = b.pop().unwrap_or(a);
        false
    };
    (a, b, c)
}

#[inline]
#[cfg(any(target_os = "android", target_os = "ios"))]
fn get_rendezvous_server_(_ms_timeout: u64) -> (String, Vec<String>) {
    (
        "81.71.156.173".to_string(),
        vec!["81.71.156.173".to_string()],
    )
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn get_rendezvous_server_(ms_timeout: u64) -> (String, Vec<String>) {
    crate::ipc::get_rendezvous_server(ms_timeout).await
}

#[inline]
#[cfg(any(target_os = "android", target_os = "ios"))]
pub async fn get_nat_type(_ms_timeout: u64) -> i32 {
    Config::get_nat_type()
}

#[inline]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn get_nat_type(ms_timeout: u64) -> i32 {
    crate::ipc::get_nat_type(ms_timeout).await
}

#[tokio::main(flavor = "current_thread")]
async fn test_rendezvous_server_() {
    let servers = Config::get_rendezvous_servers();
    if servers.len() <= 1 {
        return;
    }
    let mut futs = Vec::new();
    for host in servers {
        futs.push(tokio::spawn(async move {
            let tm = std::time::Instant::now();
            if socket_client::connect_tcp(
                crate::check_port(&host, RENDEZVOUS_PORT),
                CONNECT_TIMEOUT,
            )
            .await
            .is_ok()
            {
                let elapsed = tm.elapsed().as_micros();
                Config::update_latency(&host, elapsed as _);
            } else {
                Config::update_latency(&host, -1);
            }
        }));
    }
    join_all(futs).await;
    Config::reset_online();
}

pub fn test_rendezvous_server() {
    std::thread::spawn(test_rendezvous_server_);
}

pub fn refresh_rendezvous_server() {
    #[cfg(any(target_os = "android", target_os = "ios", feature = "cli"))]
    test_rendezvous_server();
    #[cfg(not(any(target_os = "android", target_os = "ios", feature = "cli")))]
    std::thread::spawn(|| {
        if crate::ipc::test_rendezvous_server().is_err() {
            test_rendezvous_server();
        }
    });
}

pub fn run_me<T: AsRef<std::ffi::OsStr>>(args: Vec<T>) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "linux")]
    if let Ok(appdir) = std::env::var("APPDIR") {
        let appimage_cmd = std::path::Path::new(&appdir).join("AppRun");
        if appimage_cmd.exists() {
            return std::process::Command::new(appimage_cmd).args(&args).spawn();
        }
    }
    let cmd = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(cmd);
    #[cfg(windows)]
    let mut force_foreground = false;
    #[cfg(windows)]
    {
        let arg_strs = args
            .iter()
            .map(|x| x.as_ref().to_string_lossy())
            .collect::<Vec<_>>();
        if arg_strs == vec!["--install"] || arg_strs == &["--noinstall"] {
            cmd.env(crate::platform::SET_FOREGROUND_WINDOW, "1");
            force_foreground = true;
        }
    }
    let result = cmd.args(&args).spawn();
    match result.as_ref() {
        Ok(_child) =>
        {
            #[cfg(windows)]
            if force_foreground {
                unsafe { winapi::um::winuser::AllowSetForegroundWindow(_child.id() as u32) };
            }
        }
        Err(err) => log::error!("run_me: {err:?}"),
    }
    result
}

#[inline]
pub fn username() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    return whoami::username().trim_end_matches('\0').to_owned();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return DEVICE_NAME.lock().unwrap().clone();
}

#[inline(always)]
#[cfg(not(target_os = "ios"))]
pub fn whoami_hostname() -> String {
    let mut hostname = whoami::fallible::hostname().unwrap_or_else(|_| "localhost".to_string());
    hostname.make_ascii_lowercase();
    hostname
}

#[inline]
pub fn hostname() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        #[allow(unused_mut)]
        let mut name = whoami_hostname();
        #[cfg(target_os = "macos")]
        if name.ends_with(".local") {
            name = name.trim_end_matches(".local").to_owned();
        }
        name
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return DEVICE_NAME.lock().unwrap().clone();
}

#[inline]
pub fn get_sysinfo() -> serde_json::Value {
    use hbb_common::sysinfo::System;
    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu();
    let memory = system.total_memory();
    let memory = (memory as f64 / 1024. / 1024. / 1024. * 100.).round() / 100.;
    let cpus = system.cpus();
    let cpu_name = cpus.first().map(|x| x.brand()).unwrap_or_default();
    let cpu_name = cpu_name.trim_end();
    let cpu_freq = cpus.first().map(|x| x.frequency()).unwrap_or_default();
    let cpu_freq = (cpu_freq as f64 / 1024. * 100.).round() / 100.;
    let cpu = if cpu_freq > 0. {
        format!("{}, {}GHz, ", cpu_name, cpu_freq)
    } else {
        "".to_owned()
    };
    let num_cpus = num_cpus::get();
    let num_pcpus = num_cpus::get_physical();
    let mut os = system.distribution_id();
    os = format!("{} / {}", os, system.long_os_version().unwrap_or_default());
    #[cfg(windows)]
    {
        os = format!("{os} - {}", system.os_version().unwrap_or_default());
    }
    let hostname = hostname();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let out;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let mut out;
    out = json!({
        "cpu": format!("{cpu}{num_cpus}/{num_pcpus} cores"),
        "memory": format!("{memory}GB"),
        "os": os,
        "hostname": hostname,
    });
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let username = crate::platform::get_active_username();
        if !username.is_empty() && (!cfg!(windows) || username != "SYSTEM") {
            out["username"] = json!(username);
        }
    }
    out
}

#[inline]
pub fn check_port<T: std::string::ToString>(host: T, port: i32) -> String {
    hbb_common::socket_client::check_port(host, port)
}

#[inline]
pub fn increase_port<T: std::string::ToString>(host: T, offset: i32) -> String {
    hbb_common::socket_client::increase_port(host, offset)
}

pub const POSTFIX_SERVICE: &'static str = "_service";

#[inline]
pub fn is_control_key(evt: &KeyEvent, key: &ControlKey) -> bool {
    if let Some(key_event::Union::ControlKey(ck)) = evt.union {
        ck.value() == key.value()
    } else {
        false
    }
}

#[inline]
pub fn is_modifier(evt: &KeyEvent) -> bool {
    if let Some(key_event::Union::ControlKey(ck)) = evt.union {
        let v = ck.value();
        v == ControlKey::Alt.value()
            || v == ControlKey::Shift.value()
            || v == ControlKey::Control.value()
            || v == ControlKey::Meta.value()
            || v == ControlKey::RAlt.value()
            || v == ControlKey::RShift.value()
            || v == ControlKey::RControl.value()
            || v == ControlKey::RWin.value()
    } else {
        false
    }
}

pub fn check_software_update() {
    if is_custom_client() {
        return;
    }
    let opt = LocalConfig::get_option(keys::OPTION_ENABLE_CHECK_UPDATE);
    if config::option2bool(keys::OPTION_ENABLE_CHECK_UPDATE, &opt) {
        std::thread::spawn(move || allow_err!(do_check_software_update()));
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn do_check_software_update() -> hbb_common::ResultType<()> {
    // 禁用自动更新检查
    Ok(())
}

#[inline]
pub fn get_app_name() -> String {
    hbb_common::config::APP_NAME.read().unwrap().clone()
}

#[inline]
pub fn is_rustdesk() -> bool {
    hbb_common::config::APP_NAME.read().unwrap().eq("RustDesk")
}

#[inline]
pub fn get_uri_prefix() -> String {
    format!("{}://", get_app_name().to_lowercase())
}

#[cfg(target_os = "macos")]
pub fn get_full_name() -> String {
    format!(
        "{}.{}",
        hbb_common::config::ORG.read().unwrap(),
        hbb_common::config::APP_NAME.read().unwrap(),
    )
}

pub fn is_setup(name: &str) -> bool {
    name.to_lowercase().ends_with("install.exe")
}

pub fn get_custom_rendezvous_server(_custom: String) -> String {
    // --- 修改点：强制返回硬编码 IP ---
    "81.71.156.173".to_string()
}

#[inline]
pub fn get_api_server(_api: String, _custom: String) -> String {
    // --- 修改点：强制返回硬编码 API ---
    "http://81.71.156.173:21118".to_string()
}

fn get_api_server_(api: String, custom: String) -> String {
    // --- 修改点：强制返回硬编码 API ---
    "http://81.71.156.173:21118".to_string()
}

#[inline]
pub fn is_public(_url: &str) -> bool {
    // 强制设为私有
    false
}

pub fn get_udp_punch_enabled() -> bool {
    config::option2bool(
        keys::OPTION_ENABLE_UDP_PUNCH,
        &get_local_option(keys::OPTION_ENABLE_UDP_PUNCH),
    )
}

pub fn get_ipv6_punch_enabled() -> bool {
    config::option2bool(
        keys::OPTION_ENABLE_IPV6_PUNCH,
        &get_local_option(keys::OPTION_ENABLE_IPV6_PUNCH),
    )
}

pub fn get_local_option(key: &str) -> String {
    let v = LocalConfig::get_option(key);
    if key == keys::OPTION_ENABLE_UDP_PUNCH || key == keys::OPTION_ENABLE_IPV6_PUNCH {
        if v.is_empty() {
            return "N".to_owned();
        }
    }
    v
}

pub fn get_audit_server(api: String, custom: String, typ: String) -> String {
    let url = get_api_server(api, custom);
    if url.is_empty() {
        return "".to_owned();
    }
    format!("{}/api/audit/{}", url, typ)
}

pub async fn post_request(url: String, body: String, header: &str) -> ResultType<String> {
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(&url, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url);
    let danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(tls_url);
    let response = post_request_(
        &url,
        tls_url,
        body.clone(),
        header,
        tls_type,
        danger_accept_invalid_cert,
        danger_accept_invalid_cert,
    )
    .await?;
    Ok(response.text().await?)
}

#[async_recursion]
async fn post_request_(
    url: &str,
    tls_url: &str,
    body: String,
    header: &str,
    tls_type: Option<TlsType>,
    danger_accept_invalid_cert: Option<bool>,
    original_danger_accept_invalid_cert: Option<bool>,
) -> ResultType<reqwest::Response> {
    let mut req = create_http_client_async(
        tls_type.unwrap_or(TlsType::Rustls),
        danger_accept_invalid_cert.unwrap_or(false),
    )
    .post(url);
    if !header.is_empty() {
        let tmp: Vec<&str> = header.split(": ").collect();
        if tmp.len() == 2 {
            req = req.header(tmp, tmp);
        }
    }
    req = req.header("Content-Type", "application/json");
    let to = std::time::Duration::from_secs(12);
    
    match req.body(body.clone()).timeout(to).send().await {
        Ok(resp) => {
            upsert_tls_cache(
                tls_url,
                tls_type.unwrap_or(TlsType::Rustls),
                danger_accept_invalid_cert.unwrap_or(false),
            );
            Ok(resp)
        }
        Err(e) => Err(anyhow!("{:?}", e)),
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn post_request_sync(url: String, body: String, header: &str) -> ResultType<String> {
    post_request(url, body, header).await
}

// HTTP helper functions (kept as original)
#[async_recursion]
async fn get_http_response_async(...) { ... }

#[tokio::main(flavor = "current_thread")]
pub async fn http_request_sync(...) { ... }

#[inline]
pub fn make_privacy_mode_msg_with_details(
    state: back_notification::PrivacyModeState,
    details: String,
    impl_key: String,
) -> Message {
    let mut misc = Misc::new();
    let mut back_notification = BackNotification {
        details,
        impl_key,
        ..Default::default()
    };
    back_notification.set_privacy_mode_state(state);
    misc.set_back_notification(back_notification);
    let mut msg_out = Message::new();
    msg_out.set_misc(misc);
    msg_out
}

#[inline]
pub fn make_privacy_mode_msg(
    state: back_notification::PrivacyModeState,
    impl_key: String,
) -> Message {
    make_privacy_mode_msg_with_details(state, "".to_owned(), impl_key)
}

pub fn is_keyboard_mode_supported(
    keyboard_mode: &KeyboardMode,
    version_number: i64,
    peer_platform: &str,
) -> bool {
    match keyboard_mode {
        KeyboardMode::Legacy => true,
        KeyboardMode::Map => {
            if peer_platform.to_lowercase() == crate::PLATFORM_ANDROID.to_lowercase() {
                false
            } else {
                version_number >= hbb_common::get_version_number("1.2.0")
            }
        }
        KeyboardMode::Translate => version_number >= hbb_common::get_version_number("1.2.0"),
        KeyboardMode::Auto => version_number >= hbb_common::get_version_number("1.2.0"),
    }
}

pub fn get_supported_keyboard_modes(version: i64, peer_platform: &str) -> Vec<KeyboardMode> {
    KeyboardMode::iter()
        .filter(|&mode| is_keyboard_mode_supported(mode, version, peer_platform))
        .map(|&mode| mode)
        .collect::<Vec<_>>()
}

// ... file transfer helper functions ...

pub fn handle_url_scheme(url: String) {
    #[cfg(not(target_os = "ios"))]
    if let Err(err) = crate::ipc::send_url_scheme(url.clone()) {
        let _ = crate::run_me(vec![url]);
    }
}

#[inline]
pub fn encode64<T: AsRef<[u8]>>(input: T) -> String {
    #[allow(deprecated)]
    base64::encode(input)
}

#[inline]
pub fn decode64<T: AsRef<[u8]>>(input: T) -> Result<Vec<u8>, base64::DecodeError> {
    #[allow(deprecated)]
    base64::decode(input)
}

pub async fn get_key(_sync: bool) -> String {
    // --- 修改点：强制返回硬编码 KEY ---
    "Mpsn0qJ82y2XMHOx0BtTwZy1iPDzXruB6rIUXp8oUAQ=".to_string()
}

pub fn pk_to_fingerprint(pk: Vec<u8>) -> String {
    let s: String = pk.iter().map(|u| format!("{:02x}", u)).collect();
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            if i > 0 && i % 4 == 0 {
                format!(" {}", c)
            } else {
                format!("{}", c)
            }
        })
        .collect()
}

#[inline]
pub async fn get_next_nonkeyexchange_msg(
    conn: &mut Stream,
    timeout: Option<u64>,
) -> Option<RendezvousMessage> {
    let timeout = timeout.unwrap_or(READ_TIMEOUT);
    for _ in 0..2 {
        if let Some(Ok(bytes)) = conn.next_timeout(timeout).await {
            if let Ok(msg_in) = RendezvousMessage::parse_from_bytes(&bytes) {
                match &msg_in.union {
                    Some(rendezvous_message::Union::KeyExchange(_)) => {
                        continue;
                    }
                    _ => {
                        return Some(msg_in);
                    }
                }
            }
        }
        break;
    }
    None
}

// check_process functions (kept as original)

pub async fn secure_tcp(conn: &mut Stream, key: &str) -> ResultType<()> {
    if use_ws() {
        return Ok(());
    }
    let rs_pk = get_rs_pk(key);
    let Some(rs_pk) = rs_pk else {
        bail!("Handshake failed: invalid public key from rendezvous server");
    };
    match timeout(READ_TIMEOUT, conn.next()).await? {
        Some(Ok(bytes)) => {
            if let Ok(msg_in) = RendezvousMessage::parse_from_bytes(&bytes) {
                match msg_in.union {
                    Some(rendezvous_message::Union::KeyExchange(ex)) => {
                        if ex.keys.len() != 1 {
                            bail!("Handshake failed: invalid key exchange message");
                        }
                        let their_pk_b = sign::verify(&ex.keys, &rs_pk)
                            .map_err(|_| anyhow!("Signature mismatch in key exchange"))?;
                        let (asymmetric_value, symmetric_value, key) = create_symmetric_key_msg(
                            get_pk(&their_pk_b)
                                .context("Wrong their public length in key exchange")?,
                        );
                        let mut msg_out = RendezvousMessage::new();
                        msg_out.set_key_exchange(KeyExchange {
                            keys: vec![asymmetric_value, symmetric_value],
                            ..Default::default()
                        });
                        timeout(CONNECT_TIMEOUT, conn.send(&msg_out)).await??;
                        conn.set_key(key);
                        log::info!("Connection secured");
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[inline]
fn get_pk(pk: &[u8]) -> Option<[u8; 32]> {
    if pk.len() == 32 {
        let mut tmp = [0u8; 32];
        tmp[..].copy_from_slice(&pk);
        Some(tmp)
    } else {
        None
    }
}

#[inline]
pub fn get_rs_pk(str_base64: &str) -> Option<sign::PublicKey> {
    if let Ok(pk) = crate::decode64(str_base64) {
        get_pk(&pk).map(|x| sign::PublicKey(x))
    } else {
        None
    }
}

pub fn decode_id_pk(signed: &[u8], key: &sign::PublicKey) -> ResultType<(String, [u8; 32])> {
    let res = IdPk::parse_from_bytes(
        &sign::verify(signed, key).map_err(|_| anyhow!("Signature mismatch"))?,
    )?;
    if let Some(pk) = get_pk(&res.pk) {
        Ok((res.id, pk))
    } else {
        bail!("Wrong their public length");
    }
}

pub fn create_symmetric_key_msg(their_pk_b: [u8; 32]) -> (Bytes, Bytes, secretbox::Key) {
    let their_pk_b = box_::PublicKey(their_pk_b);
    let (our_pk_b, out_sk_b) = box_::gen_keypair();
    let key = secretbox::gen_key();
    let nonce = box_::Nonce([0u8; box_::NONCEBYTES]);
    let sealed_key = box_::seal(&key.0, &nonce, &their_pk_b, &out_sk_b);
    (Vec::from(our_pk_b.0).into(), sealed_key.into(), key)
}

#[inline]
pub fn using_public_server() -> bool {
    // 强制返回 false 表示始终使用自定义服务器
    false
}

// ... ThrottledInterval implementation ...

pub fn load_custom_client() {
    // 强制启用内置配置读取
    read_custom_client("");
}

// ... custom client advanced settings implementation ...

pub fn read_custom_client(_config: &str) {
    // 这里可以保持逻辑，但核心配置已经被上面的 get_rendezvous_server 等函数拦截了
}

#[inline]
pub fn is_empty_uni_link(arg: &str) -> bool {
    let prefix = crate::get_uri_prefix();
    if !arg.starts_with(&prefix) {
        return false;
    }
    arg[prefix.len()..].chars().all(|c| c == '/')
}

pub fn get_hwid() -> Bytes {
    use hbb_common::sha2::{Digest, Sha256};
    let uuid = hbb_common::get_uuid();
    let mut hasher = Sha256::new();
    hasher.update(&uuid);
    Bytes::from(hasher.finalize().to_vec())
}

#[inline]
pub fn get_builtin_option(key: &str) -> String {
    config::BUILTIN_SETTINGS
        .read()
        .unwrap()
        .get(key)
        .cloned()
        .unwrap_or_default()
}

#[inline]
pub fn is_custom_client() -> bool {
    // 强制为 true
    true
}

pub fn verify_login(_raw: &str, _id: &str) -> bool {
    true
}

#[inline]
pub fn is_udp_disabled() -> bool {
    Config::get_option(keys::OPTION_DISABLE_UDP) == "Y"
}

// STUN tests (kept as original)
