#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use core::fmt::Write as _;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources, dns::DnsQueryType, tcp::client::{TcpClient, TcpClientState}, udp::{PacketMetadata, UdpSocket}};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read, Write as AsyncWrite};
use embedded_nal_async::TcpConnect;
use esp_hal::{Config, clock::CpuClock, i2c::master::{Config as I2cConfig, I2c}, rng::Rng, rmt::Rmt, time::Rate, timer::timg::TimerGroup};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, WS2812B_TIMING};
use esp_println::println;
use esp_radio::wifi::{AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, PowerSaveMode, WifiController, sta::StationConfig};
use heapless::{String, Vec};
use smart_leds_trait::{RGB8, SmartLedsWrite};

#[path = "../secrets.rs"]
mod secrets;

const DEVICE_ID: &str = secrets::DEVICE_ID;
const TOPIC: &str = secrets::MQTT_TOPIC;
const LATITUDE: &str = secrets::LATITUDE;
const LONGITUDE: &str = secrets::LONGITUDE;
const TEMPERATURE_OFFSET_CENTI: i32 = secrets::TEMPERATURE_OFFSET_CENTI;

const LED_BOOT: u8 = 0;
const LED_WIFI_CONNECTING: u8 = 1;
const LED_WIFI_LOST: u8 = 2;
const LED_MQTT_CONNECTING: u8 = 3;
const LED_PUBLISH_OK: u8 = 4;
const LED_MQTT_ERROR: u8 = 5;
const LED_SENSOR_ERROR: u8 = 6;
const LED_NORMAL: u8 = 7;

static LED_STATE: AtomicU8 = AtomicU8::new(LED_BOOT);
static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);
static WIFI_RECONNECTS: AtomicU32 = AtomicU32::new(0);
static MQTT_RECONNECTS: AtomicU32 = AtomicU32::new(0);
static NTP_UNIX_SECONDS: AtomicU32 = AtomicU32::new(0);
static NTP_SYNC_MILLIS: AtomicU32 = AtomicU32::new(0);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let led = RmtSmartLeds::<{ buffer_size::<RGB8>(1) }, _, RGB8, color_order::Grb>::new(
        WS2812B_TIMING,
        rmt.channel0,
        peripherals.GPIO2,
        Rate::from_mhz(80),
    )
    .unwrap();
    spawner.spawn(led_task(led).unwrap());

    let wifi_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(secrets::WIFI_SSID.try_into().unwrap())
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                secrets::WIFI_PASSWORD.try_into().unwrap(),
            )),
    );
    let wifi_interface = Interface::station();
    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(wifi_config),
    )
    .unwrap();
    controller.set_power_saving(PowerSaveMode::Minimum).unwrap();

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );

    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    println!("aguardando Wi-Fi/DHCP");
    stack.wait_config_up().await;
    LED_STATE.store(LED_NORMAL, Ordering::Relaxed);
    println!("Wi-Fi/DHCP conectado");
    sync_ntp(stack).await;

    let tcp_client = TcpClient::new(
        stack,
        mk_static!(TcpClientState<1, 1536, 512>, TcpClientState::<1, 1536, 512>::new()),
    );

    let mut i2c = I2c::new(peripherals.I2C0, I2cConfig::default())
        .unwrap()
        .with_sda(peripherals.GPIO7)
        .with_scl(peripherals.GPIO8);
    for address in 0x08..0x78 {
        if i2c.write(address, &[]).is_ok() {
            println!("I2C encontrado: 0x{:02x}", address);
        }
    }
    let mut sensor = shtcx::shtc3(i2c);
    let mut sequence: u32 = 0;
    println!("{} iniciado", DEVICE_ID);

    loop {
        if sequence % 60 == 0 {
            sync_ntp(stack).await;
        }
        println!("iniciando leitura SHTC3");
        let (temperature_centi, humidity_centi, sensor_ok) = match shtc3_read(&mut sensor).await {
            Ok(measurement) => {
                println!("SHTC3 lido");
                (measurement.0 + TEMPERATURE_OFFSET_CENTI, measurement.1, true)
            }
            Err(error) => {
                println!("falha SHTC3 em GPIO7/GPIO8: {:?}", error);
                LED_STATE.store(LED_SENSOR_ERROR, Ordering::Relaxed);
                (0, 0, false)
            }
        };

        if !sensor_ok {
            LED_STATE.store(LED_SENSOR_ERROR, Ordering::Relaxed);
        } else if !WIFI_CONNECTED.load(Ordering::Relaxed) {
            LED_STATE.store(LED_WIFI_LOST, Ordering::Relaxed);
        } else {
            LED_STATE.store(LED_MQTT_CONNECTING, Ordering::Relaxed);
        }

        let mut payload: String<512> = String::new();
        let mut time_fields: String<128> = String::new();
        if let Some(unix_seconds) = current_unix_seconds() {
            let (year, month, day, hour, minute, second) = unix_to_calendar(unix_seconds);
            let _ = write!(time_fields, "\"timestamp_unix\":{},\"ano\":{},\"mes\":{},\"dia\":{},\"hora\":{},\"minuto\":{},\"segundo\":{},\"time_status\":\"synchronized\"", unix_seconds, year, month, day, hour, minute, second);
        } else {
            let _ = write!(time_fields, "\"timestamp_unix\":null,\"ano\":null,\"mes\":null,\"dia\":null,\"hora\":null,\"minuto\":null,\"segundo\":null,\"time_status\":\"unsynchronized\"");
        }

        if WIFI_CONNECTED.load(Ordering::Relaxed) {
            if let Ok(mut connection) = tcp_client
            .connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(secrets::MQTT_HOST)), secrets::MQTT_PORT))
            .await
            {
                println!("TCP conectado ao MQTT");
                if mqtt_connect(&mut connection).await.is_ok() {
                    println!("MQTT conectado");
                    if sensor_ok {
                        let _ = write!(payload, "{{\"device_id\":\"{}\",\"sequence\":{},\"temperature_c\":{}.{:02},\"humidity_percent\":{}.{:02},\"sensor_status\":\"ok\",\"wifi_status\":\"connected\",\"mqtt_status\":\"connected\",\"led_state\":\"publish_ok\",\"publish_ok\":true,\"wifi_reconnects\":{},\"mqtt_reconnects\":{},\"latitude\":{},\"longitude\":{},\"interval_s\":60,{} }}", DEVICE_ID, sequence, temperature_centi / 100, temperature_centi.unsigned_abs() % 100, humidity_centi / 100, humidity_centi.unsigned_abs() % 100, WIFI_RECONNECTS.load(Ordering::Relaxed), MQTT_RECONNECTS.load(Ordering::Relaxed), LATITUDE, LONGITUDE, time_fields);
                    } else {
                        let _ = write!(payload, "{{\"device_id\":\"{}\",\"sequence\":{},\"temperature_c\":null,\"humidity_percent\":null,\"sensor_status\":\"hardware_not_connected\",\"wifi_status\":\"connected\",\"mqtt_status\":\"connected\",\"led_state\":\"sensor_error\",\"publish_ok\":true,\"wifi_reconnects\":{},\"mqtt_reconnects\":{},\"latitude\":{},\"longitude\":{},\"interval_s\":60,{} }}", DEVICE_ID, sequence, WIFI_RECONNECTS.load(Ordering::Relaxed), MQTT_RECONNECTS.load(Ordering::Relaxed), LATITUDE, LONGITUDE, time_fields);
                    }
                    if mqtt_publish(&mut connection, TOPIC, payload.as_bytes()).await.is_ok() {
                        LED_STATE.store(LED_PUBLISH_OK, Ordering::Relaxed);
                        println!("telemetria publicada");
                    } else {
                        LED_STATE.store(LED_MQTT_ERROR, Ordering::Relaxed);
                    }
                } else {
                    increment(&MQTT_RECONNECTS);
                    LED_STATE.store(LED_MQTT_ERROR, Ordering::Relaxed);
                }
                let _ = connection.flush().await;
            } else {
                increment(&MQTT_RECONNECTS);
                LED_STATE.store(LED_MQTT_ERROR, Ordering::Relaxed);
            }
        }

        sequence = sequence.wrapping_add(1);
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        LED_STATE.store(LED_WIFI_CONNECTING, Ordering::Relaxed);
        if controller.connect_async().await.is_err() {
            WIFI_CONNECTED.store(false, Ordering::Relaxed);
            increment(&WIFI_RECONNECTS);
            LED_STATE.store(LED_WIFI_LOST, Ordering::Relaxed);
            Timer::after(Duration::from_secs(5)).await;
        } else {
            WIFI_CONNECTED.store(true, Ordering::Relaxed);
            LED_STATE.store(LED_NORMAL, Ordering::Relaxed);
            controller.wait_for_disconnect_async().await.ok();
            WIFI_CONNECTED.store(false, Ordering::Relaxed);
            increment(&WIFI_RECONNECTS);
            LED_STATE.store(LED_WIFI_LOST, Ordering::Relaxed);
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await;
}

fn increment(counter: &AtomicU32) {
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.saturating_add(1), Ordering::Relaxed);
}

#[embassy_executor::task]
async fn led_task(mut led: RgbLed) {
    loop {
        match LED_STATE.load(Ordering::Relaxed) {
            LED_BOOT => {
                for _ in 0..3 {
                    show_color(&mut led, RGB8 { r: 255, g: 255, b: 255 });
                    Timer::after(Duration::from_millis(1000)).await;
                    show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(500)).await;
                }
                LED_STATE.store(LED_WIFI_CONNECTING, Ordering::Relaxed);
            }
            LED_WIFI_CONNECTING => {
                show_color(&mut led, RGB8 { r: 0, g: 0, b: 80 });
                Timer::after(Duration::from_millis(500)).await;
                show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                Timer::after(Duration::from_millis(1500)).await;
            }
            LED_WIFI_LOST => {
                for _ in 0..3 {
                    show_color(&mut led, RGB8 { r: 255, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(100)).await;
                    show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(100)).await;
                }
                Timer::after(Duration::from_millis(700)).await;
            }
            LED_MQTT_CONNECTING => {
                for _ in 0..2 {
                    show_color(&mut led, RGB8 { r: 255, g: 120, b: 0 });
                    Timer::after(Duration::from_millis(100)).await;
                    show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(150)).await;
                }
                Timer::after(Duration::from_millis(1600)).await;
            }
            LED_PUBLISH_OK => {
                show_color(&mut led, RGB8 { r: 0, g: 255, b: 0 });
                Timer::after(Duration::from_millis(500)).await;
                show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                LED_STATE.store(LED_NORMAL, Ordering::Relaxed);
            }
            LED_MQTT_ERROR => {
                for _ in 0..2 {
                    show_color(&mut led, RGB8 { r: 255, g: 80, b: 0 });
                    Timer::after(Duration::from_millis(180)).await;
                    show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(180)).await;
                }
                Timer::after(Duration::from_millis(2460)).await;
            }
            LED_SENSOR_ERROR => {
                for _ in 0..3 {
                    show_color(&mut led, RGB8 { r: 180, g: 0, b: 255 });
                    Timer::after(Duration::from_millis(100)).await;
                    show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
                    Timer::after(Duration::from_millis(100)).await;
                }
                Timer::after(Duration::from_millis(4700)).await;
            }
            _ => {
                show_color(&mut led, RGB8 { r: 0, g: 20, b: 20 });
                for _ in 0..20 {
                    if LED_STATE.load(Ordering::Relaxed) != LED_NORMAL {
                        break;
                    }
                    Timer::after(Duration::from_millis(100)).await;
                }
                show_color(&mut led, RGB8 { r: 0, g: 0, b: 0 });
            }
        }
    }
}

type RgbLed = RmtSmartLeds<'static, { buffer_size::<RGB8>(1) }, esp_hal::Blocking, RGB8, color_order::Grb>;

fn show_color(led: &mut RgbLed, color: RGB8) {
    let _ = led.write([color]);
}

async fn sync_ntp(stack: embassy_net::Stack<'static>) {
    let ntp_addresses = match stack.dns_query("pool.ntp.org", DnsQueryType::A).await {
        Ok(addresses) if !addresses.is_empty() => addresses,
        _ => {
            println!("falha ao resolver pool.ntp.org");
            return;
        }
    };

    let mut rx_metadata = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0u8; 256];
    let mut tx_metadata = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0u8; 128];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_metadata,
        &mut rx_buffer,
        &mut tx_metadata,
        &mut tx_buffer,
    );
    if socket.bind(0).is_err() {
        println!("falha ao abrir socket NTP");
        return;
    }

    let mut request = [0u8; 48];
    request[0] = 0x1b;
    let endpoint = (ntp_addresses[0], 123);
    if socket.send_to(&request, endpoint).await.is_err() {
        println!("falha ao enviar pedido NTP");
        return;
    }

    let mut response = [0u8; 64];
    let (length, _) = match socket.recv_from(&mut response).await {
        Ok(result) => result,
        Err(_) => {
            println!("falha ao receber resposta NTP");
            return;
        }
    };
    if length < 48 {
        println!("resposta NTP invalida");
        return;
    }

    let ntp_seconds = u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
    const NTP_UNIX_OFFSET: u32 = 2_208_988_800;
    if ntp_seconds < NTP_UNIX_OFFSET {
        println!("timestamp NTP invalido");
        return;
    }
    NTP_UNIX_SECONDS.store(ntp_seconds - NTP_UNIX_OFFSET, Ordering::Relaxed);
    NTP_SYNC_MILLIS.store(Instant::now().as_millis() as u32, Ordering::Relaxed);
    println!("hora sincronizada via NTP");
}

fn current_unix_seconds() -> Option<u32> {
    let sync_millis = NTP_SYNC_MILLIS.load(Ordering::Relaxed);
    if sync_millis == 0 {
        return None;
    }
    let elapsed_seconds = (Instant::now().as_millis() as u32).wrapping_sub(sync_millis) / 1000;
    Some(NTP_UNIX_SECONDS.load(Ordering::Relaxed).wrapping_add(elapsed_seconds))
}

fn unix_to_calendar(unix_seconds: u32) -> (i32, u32, u32, u32, u32, u32) {
    let seconds_of_day = unix_seconds % 86_400;
    let days = (unix_seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (
        year as i32,
        month as u32,
        day as u32,
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
    )
}

async fn mqtt_connect<T>(connection: &mut T) -> Result<(), ()>
where
    T: AsyncWrite + Read,
{
    let mut body: Vec<u8, 256> = Vec::new();
    put_utf8(&mut body, "MQTT")?;
    body.push(4).map_err(|_| ())?;
    body.push(0xC2).map_err(|_| ())?;
    body.push(0).map_err(|_| ())?;
    body.push(60).map_err(|_| ())?;
    put_utf8(&mut body, DEVICE_ID)?;
    put_utf8(&mut body, secrets::MQTT_USERNAME)?;
    put_utf8(&mut body, secrets::MQTT_PASSWORD)?;
    send_packet(connection, 0x10, &body).await?;

    let mut response = [0u8; 4];
    connection.read_exact(&mut response).await.map_err(|_| ())?;
    if response[0] != 0x20 || response[3] != 0 {
        return Err(());
    }
    Ok(())
}

async fn mqtt_publish<T>(connection: &mut T, topic: &str, payload: &[u8]) -> Result<(), ()>
where
    T: AsyncWrite + Read,
{
    let mut body: Vec<u8, 512> = Vec::new();
    put_utf8(&mut body, topic)?;
    for byte in payload {
        body.push(*byte).map_err(|_| ())?;
    }
    send_packet(connection, 0x30, &body).await
}

async fn send_packet<T>(connection: &mut T, header: u8, body: &[u8]) -> Result<(), ()>
where
    T: AsyncWrite,
{
    let mut packet: Vec<u8, 600> = Vec::new();
    packet.push(header).map_err(|_| ())?;
    let mut length = body.len();
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        packet.push(byte).map_err(|_| ())?;
        if length == 0 {
            break;
        }
    }
    for byte in body {
        packet.push(*byte).map_err(|_| ())?;
    }
    connection.write_all(&packet).await.map_err(|_| ())
}

fn put_utf8<const N: usize>(buffer: &mut Vec<u8, N>, value: &str) -> Result<(), ()> {
    let length = u16::try_from(value.len()).map_err(|_| ())?;
    buffer.extend_from_slice(&length.to_be_bytes()).map_err(|_| ())?;
    buffer.extend_from_slice(value.as_bytes()).map_err(|_| ())
}

async fn shtc3_read<I>(sensor: &mut shtcx::ShtC3<I>) -> Result<(i32, i32), shtcx::Error<I::Error>>
where
    I: embedded_hal::i2c::I2c,
{
    use shtcx::LowPower;

    sensor.start_wakeup()?;
    Timer::after(Duration::from_millis(1)).await;
    sensor.start_measurement(shtcx::PowerMode::NormalMode)?;
    Timer::after(Duration::from_millis(20)).await;
    let measurement = sensor.get_measurement_result()?;
    Ok((
        measurement.temperature.as_millidegrees_celsius() / 10,
        measurement.humidity.as_millipercent() / 10,
    ))
}
