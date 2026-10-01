.PHONY: check build flash monitor deploy

PORT ?= /dev/ttyACM0
ELF := target/riscv32imc-unknown-none-elf/release/rust-esp32-2-mqtt-sensores

check:
	cargo check

build:
	cargo build --release

flash: build
	espflash flash --chip esp32c3 --port $(PORT) --non-interactive --skip-update-check $(ELF)

monitor:
	espflash monitor --chip esp32c3 --port $(PORT) --skip-update-check

deploy: flash monitor
