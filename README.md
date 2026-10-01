# ESP32-C3-DevKit-RUST-2 MQTT Sensor

Firmware Rust específico para a placa **ESP32-C3-DevKit-RUST-2 Rev. 2.0**.

O firmware:

- Conecta ao Wi-Fi.
- Lê o SHTC3 em `0x70`.
- Detecta o IMU ICM-42670-P em `0x68`.
- Publica temperatura, umidade, localização e estado de rede via MQTT.
- Publica a cada 60 segundos.
- Sincroniza o relógio via `pool.ntp.org` depois do DHCP e periodicamente.
- Usa o WS2812 RGB da placa no GPIO2 para indicar estados.
- Reconecta ao Wi-Fi e ao MQTT.

## Hardware

| Função | GPIO/endereço |
|---|---|
| SHTC3 SDA | GPIO7 |
| SHTC3 SCL | GPIO8 |
| SHTC3 | I2C `0x70` |
| ICM-42670-P | I2C `0x68` |
| WS2812 RGB | GPIO2 |
| LED comum | GPIO10 |
| Botão BOOT | GPIO9 |

O projeto é exclusivo da `ESP32-C3-DevKit-RUST-2`. Não use os GPIOs de outra placa sem revisar o esquemático.

## Linux: dependências

```bash
sudo apt update
sudo apt install -y build-essential curl git pkg-config libudev-dev mosquitto-clients
```

Instale Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Instale a toolchain ESP-Rust:

```bash
cargo install espup espflash
espup install
source "$HOME/export-esp.sh"
```

Para carregar o ambiente ESP em novos terminais:

```bash
source "$HOME/.cargo/env"
source "$HOME/export-esp.sh"
```

## USB

Adicione seu usuário ao grupo serial:

```bash
sudo usermod -aG dialout "$USER"
```

Saia e entre novamente na sessão. Conecte a placa com um cabo USB-C de dados e confirme:

```bash
lsusb
ls -l /dev/ttyACM* /dev/serial/by-id/*
```

Normalmente a placa aparece como `/dev/ttyACM0`.

Confirme o chip:

```bash
espflash board-info --chip esp32c3 --port /dev/ttyACM0
```

## Configuração local

O arquivo `src/secrets.rs` não é versionado. Crie-o a partir do exemplo:

```bash
cp src/secrets.example.rs src/secrets.rs
```

Edite os valores:

```rust
pub const DEVICE_ID: &str = "iot-sensor-002";
pub const MQTT_TOPIC: &str = "sensores/iot-sensor-002/telemetria";
pub const WIFI_SSID: &str = "iot";
pub const WIFI_PASSWORD: &str = "your-wifi-password";
pub const MQTT_HOST: [u8; 4] = [38, 172, 195, 113];
pub const MQTT_PORT: u16 = 1883;
pub const MQTT_USERNAME: &str = "iot-sensor-002";
pub const MQTT_PASSWORD: &str = "your-mqtt-password";
pub const TEMPERATURE_OFFSET_CENTI: i32 = -476;
```

Nunca publique `src/secrets.rs`. O repositório contém somente `src/secrets.example.rs`.

## Compilar

```bash
source "$HOME/.cargo/env"
source "$HOME/export-esp.sh"
cargo check
cargo build --release
```

Ou:

```bash
make check
make build
```

## Gravar na placa

Com a placa em `/dev/ttyACM0`:

```bash
make flash
```

Outra porta:

```bash
make flash PORT=/dev/ttyACM1
```

Comando equivalente:

```bash
espflash flash \
  --chip esp32c3 \
  --port /dev/ttyACM0 \
  --non-interactive \
  --skip-update-check \
  target/riscv32imc-unknown-none-elf/release/rust-esp32-2-mqtt-sensores
```

## Monitor serial

```bash
make monitor PORT=/dev/ttyACM0
```

Saia com `Ctrl+C` ou a combinação indicada pelo `espflash`.

Saída esperada:

```text
I2C encontrado: 0x68
I2C encontrado: 0x70
SHTC3 lido
MQTT conectado
telemetria publicada
```

## Cores do RGB

- Branco: inicialização.
- Azul: conectando ao Wi-Fi.
- Vermelho: Wi-Fi perdido.
- Laranja: conectando ao MQTT.
- Verde: publicação bem-sucedida.
- Laranja forte: erro MQTT.
- Roxo: erro no SHTC3.
- Ciano fraco: funcionamento normal.

## Ler MQTT

```bash
mosquitto_sub \
  -h 38.172.195.113 \
  -p 1883 \
  -u 'iot-sensor-002' \
  -P 'your-mqtt-password' \
  -t 'sensores/iot-sensor-002/#' \
  -v
```

O payload inclui `temperature_c`, `humidity_percent`, `sensor_status`, `wifi_status`, `mqtt_status`, `led_state`, contadores de reconexão e localização.

O `timestamp_unix` é UTC. Os campos separados de data e hora são convertidos para o horário local de Campinas, `America/Sao_Paulo` (`UTC-3`):

```json
{
  "timestamp_unix": 1790868277,
  "ano": 2026,
  "mes": 10,
  "dia": 1,
  "hora": 15,
  "minuto": 24,
  "segundo": 37,
  "timezone": "America/Sao_Paulo",
  "utc_offset_hours": -3,
  "time_status": "synchronized"
}
```

O dispositivo sincroniza via DNS com `pool.ntp.org` após obter um endereço IP. Se o NTP estiver indisponível, os campos de tempo ficam `null` e `time_status` será `unsynchronized`; a telemetria continua sendo publicada.

## Problemas comuns

### A placa não aparece

Use cabo USB-C de dados, confirme `/dev/ttyACM0` e verifique o grupo `dialout`.

### Falha ao entrar no bootloader

Segure BOOT, pressione RESET, solte RESET e depois solte BOOT. Execute o flash novamente.

### SHTC3 não encontrado

Confirme no monitor:

```text
I2C encontrado: 0x70
```

Na RUST-2 Rev. 2.0, SDA é GPIO7 e SCL é GPIO8. Não troque esses pinos por GPIO10.

### MQTT não conecta

Confirme IP, porta, usuário, senha, Wi-Fi e permissões do usuário no broker. A porta `1883` não usa TLS.

## Limpeza

```bash
cargo clean
```

## Licença

Adicione aqui a licença escolhida para o projeto.
