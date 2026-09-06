# Parse failures — eval corpora

Files that fail tree-sitter parsing during `trace analyze`. Regenerate TSV with:

```bash
set -euo pipefail   # stop at the first failure, do not run on with stale inputs

# One corpus base for every step: fetch_corpora.py, the analyze runs
# below and this script all read $TRACE_CORPUS_BASE.
export TRACE_CORPUS_BASE=/private/tmp/corpora

python3 scripts/fetch_corpora.py   # corpora at the revisions pinned in scripts/eval_expected.json
cargo build --release -p trace-cli && cargo build --release -p trace-cli --examples

# One analyze + one TSV per corpus, read back by these exact names.
# Each TSV is written to a .part file and renamed only if the
# command succeeded, so a failed run leaves no final file at all
# -- the generator treats a MISSING file as an error and an EMPTY
# one as a corpus with zero parse failures. Clear stale files first.
rm -f /tmp/parse_failures_{hdf,hiview,camera}.tsv{,.part}
target/release/trace analyze "$TRACE_CORPUS_BASE/drivers_hdf_core" -o /tmp/hdf_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/drivers_hdf_core" --from-db /tmp/hdf_parse_check.db > /tmp/parse_failures_hdf.tsv.part
mv /tmp/parse_failures_hdf.tsv.part /tmp/parse_failures_hdf.tsv
target/release/trace analyze "$TRACE_CORPUS_BASE/hiviewdfx_hiview" -o /tmp/hiview_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/hiviewdfx_hiview" --from-db /tmp/hiview_parse_check.db > /tmp/parse_failures_hiview.tsv.part
mv /tmp/parse_failures_hiview.tsv.part /tmp/parse_failures_hiview.tsv
target/release/trace analyze "$TRACE_CORPUS_BASE/multimedia_camera_framework" -o /tmp/camera_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/multimedia_camera_framework" --from-db /tmp/camera_parse_check.db > /tmp/parse_failures_camera.tsv.part
mv /tmp/parse_failures_camera.tsv.part /tmp/parse_failures_camera.tsv

python3 scripts/gen_parse_failures_report.py
```

The `parse_failures` example re-preprocesses with whatever build runs it; the DB only selects the failing-file set. Build the binary *and* the examples (the `--examples` flag alone leaves `target/release/trace` stale).

## Overview

| Corpus | Root | Failing files | Top category |
|--------|------|--------------:|--------------|
| `drivers_hdf_core` | `/private/tmp/corpora/drivers_hdf_core` | 163 | generic ERROR nodes (mixed C++ constructs) |
| `hiviewdfx_hiview` | `/private/tmp/corpora/hiviewdfx_hiview` | 32 | generic ERROR nodes (mixed C++ constructs) |
| `multimedia_camera_framework` | `/private/tmp/corpora/multimedia_camera_framework` | 64 | generic ERROR nodes (mixed C++ constructs) |

## Cross-corpus category totals

| Category | HDF | Hiview | Camera | Total |
|----------|----:|-------:|-------:|------:|
| generic ERROR nodes (mixed C++ constructs) | 160 | 19 | 43 | 222 |
| gtest/HWTEST macros (`missing ;`) | 2 | 1 | 18 | 21 |
| missing type identifiers (often macro-expanded types) | 1 | 7 | 2 | 10 |
| other / mixed | 0 | 3 | 1 | 4 |
| extern template instantiations | 0 | 2 | 0 | 2 |

## drivers_hdf_core

Generated from `trace analyze /private/tmp/corpora/drivers_hdf_core` (163 files with parse warnings).
Each entry is a translation unit or header indexed as its own file; reasons come from tree-sitter ERROR sites in preprocessed source.

**Total failing files:** 163

### Failure categories

| Category | Files |
|----------|------:|
| generic ERROR nodes (mixed C++ constructs) | 160 |
| gtest/HWTEST macros (`missing ;`) | 2 |
| missing type identifiers (often macro-expanded types) | 1 |

### File list

| # | File | Reason | Error sites |
|---|------|--------|-------------|
| 1 | `adapter/khdf/linux/manager/src/hdf_kevent.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 2 | `adapter/khdf/linux/model/camera/src/contig_dma.c` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 3 | `adapter/khdf/linux/model/camera/src/sg_dma.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 4 | `adapter/khdf/linux/model/camera/src/virtual_malloc.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 5 | `adapter/khdf/linux/model/storage/emmc_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 6 | `adapter/khdf/linux/model/storage/sdio_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 7 | `adapter/khdf/linux/model/usb/host/src/usb_net_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 8 | `adapter/khdf/linux/model/usb/host/src/usb_pnp_notify.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 9 | `adapter/khdf/linux/platform/adc/adc_iio_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 10 | `adapter/khdf/linux/platform/clock/clock_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 11 | `adapter/khdf/linux/platform/gpio/gpio_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 12 | `adapter/khdf/linux/platform/i2c/i2c_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 13 | `adapter/khdf/linux/platform/mipi_csi/mipi_v4l2_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 14 | `adapter/khdf/linux/platform/mipi_dsi/mipi_drm_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 15 | `adapter/khdf/linux/platform/mipi_dsi/mipi_tx_dev.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 16 | `adapter/khdf/linux/platform/mipi_dsi/mipi_tx_hi35xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 17 | `adapter/khdf/linux/platform/pwm/pwm_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 18 | `adapter/khdf/linux/platform/regulator/regulator_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 19 | `adapter/khdf/linux/platform/rtc/rtc_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 20 | `adapter/khdf/linux/platform/spi/spi_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 21 | `adapter/khdf/linux/platform/uart/uart_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 22 | `adapter/khdf/linux/platform/watchdog/watchdog_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 23 | `adapter/khdf/linux/test/platform/i2c/i2c_adapter_dummy.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 24 | `adapter/khdf/liteos/model/storage/src/mmc/mmc_block_lite.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 12 |
| 25 | `adapter/khdf/liteos/model/storage/src/mtd/mtd_block_lite.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 9 |
| 26 | `adapter/khdf/liteos/model/storage/src/mtd/mtd_char_lite.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 13 |
| 27 | `adapter/khdf/liteos/model/usb/host/src/usb_pnp_manager.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 28 | `adapter/khdf/liteos/model/usb/host/src/usb_pnp_notify.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 29 | `adapter/khdf/liteos/model/usb/host/src/usb_test_pnp_notify.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 30 | `adapter/khdf/liteos_m/test/sample_driver/src/sample_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 31 | `adapter/khdf/uniproton/test/sample_driver/src/platform_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 32 | `adapter/platform/can/can_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 33 | `adapter/platform/gpio/gpio_asr.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 34 | `adapter/platform/gpio/gpio_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 35 | `adapter/platform/gpio/gpio_gr5xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 36 | `adapter/platform/gpio/gpio_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 37 | `adapter/platform/gpio/gpio_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 38 | `adapter/platform/i2c/i2c_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 39 | `adapter/platform/i2c/i2c_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 40 | `adapter/platform/i2c/i2c_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 41 | `adapter/platform/mipi_dsi/mipi_drm_imx8mm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 42 | `adapter/platform/pwm/pwm_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 43 | `adapter/platform/pwm/pwm_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 44 | `adapter/platform/pwm/pwm_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 45 | `adapter/platform/spi/spi_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 46 | `adapter/platform/spi/spi_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 47 | `adapter/platform/spi/spi_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 48 | `adapter/platform/uart/uart_asr.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 49 | `adapter/platform/uart/uart_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 50 | `adapter/platform/uart/uart_gr5xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 51 | `adapter/platform/uart/uart_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 52 | `adapter/platform/uart/uart_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 53 | `adapter/platform/watchdog/watchdog_bes.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 54 | `adapter/platform/watchdog/watchdog_stm32f4xx.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 55 | `adapter/platform/watchdog/watchdog_wm.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 56 | `adapter/uhdf2/host/test/unittest/sample1_driver/sample1_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 57 | `adapter/uhdf2/host/test/unittest/sample_driver/sample_driver.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 4 |
| 58 | `adapter/uhdf2/osal/test/unittest/common/sample_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 59 | `framework/core/manager/test/unittest/common/hdf_sbuf_test.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 60 | `framework/model/audio/core/src/audio_host.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 61 | `framework/model/audio/dispatch/src/audio_control_dispatch.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 62 | `framework/model/audio/dispatch/src/audio_stream_dispatch.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 63 | `framework/model/audio/hdmi/src/audio_hdmi_codec_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 64 | `framework/model/audio/usb/src/audio_usb_codec_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 65 | `framework/model/audio/usb/src/audio_usb_dma_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 66 | `framework/model/audio/usb/src/audio_usb_endpoints.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 67 | `framework/model/audio/usb/src/audio_usb_mixer.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 5 |
| 68 | `framework/model/display/driver/adapter_soc/hi35xx_disp.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 69 | `framework/model/display/driver/backlight/hdf_bl.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 70 | `framework/model/display/driver/backlight/pwm_bl.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 71 | `framework/model/display/driver/hdf_disp.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 72 | `framework/model/display/driver/hdf_drm_panel.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 9 |
| 73 | `framework/model/display/driver/lcdkit/lite_lcdkit.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 74 | `framework/model/display/driver/panel/ili9881_st_5p5.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 75 | `framework/model/display/driver/panel/ili9881c_boe.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 76 | `framework/model/display/driver/panel/mipi_icn9700.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 77 | `framework/model/display/driver/panel/ssp_st7789.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 78 | `framework/model/input/driver/hdf_encoder.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 79 | `framework/model/input/driver/hdf_encoder.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 80 | `framework/model/input/driver/hdf_hid_adapter.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 81 | `framework/model/input/driver/hdf_infrared.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 82 | `framework/model/input/driver/hdf_input_device_manager.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 83 | `framework/model/input/driver/hdf_key.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 84 | `framework/model/input/driver/hdf_touch.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 85 | `framework/model/input/driver/touchscreen/touch_ft5406.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 86 | `framework/model/input/driver/touchscreen/touch_ft5x06.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 87 | `framework/model/input/driver/touchscreen/touch_ft6336.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 88 | `framework/model/input/driver/touchscreen/touch_gt911.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 89 | `framework/model/misc/dsoftbus/src/hdf_dsoftbus_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 90 | `framework/model/misc/light/driver/src/light_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 91 | `framework/model/misc/vibrator/driver/src/vibrator_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 92 | `framework/model/network/ethernet/src/hdf_eth_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 93 | `framework/model/network/wifi/core/hdf_wifi_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 94 | `framework/model/sensor/driver/accel/sensor_accel_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 95 | `framework/model/sensor/driver/accel/sensor_gravity_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 96 | `framework/model/sensor/driver/als/sensor_als_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 97 | `framework/model/sensor/driver/barometer/sensor_barometer_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 98 | `framework/model/sensor/driver/common/src/sensor_device_manager.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 99 | `framework/model/sensor/driver/gas/sensor_gas_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 100 | `framework/model/sensor/driver/gyro/sensor_gyro_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 101 | `framework/model/sensor/driver/hall/sensor_hall_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 102 | `framework/model/sensor/driver/humidity/sensor_humidity_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 103 | `framework/model/sensor/driver/magnetic/sensor_magnetic_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 104 | `framework/model/sensor/driver/pedometer/sensor_pedometer_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 105 | `framework/model/sensor/driver/ppg/sensor_ppg_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 106 | `framework/model/sensor/driver/proximity/sensor_proximity_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 107 | `framework/model/sensor/driver/temperature/sensor_temperature_driver.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 108 | `framework/sample/platform/uart/src/uart_sample.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 109 | `framework/support/platform/src/adc/adc_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 110 | `framework/support/platform/src/clock/clock_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 111 | `framework/support/platform/src/dac/dac_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 112 | `framework/support/platform/src/gpio/gpio_service.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 113 | `framework/support/platform/src/i2c/i2c_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 114 | `framework/support/platform/src/i3c/i3c_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 115 | `framework/support/platform/src/pin/pin_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 116 | `framework/support/platform/src/regulator/regulator_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 117 | `framework/support/platform/src/timer/timer_core.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 118 | `framework/test/unittest/common/hdf_main_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 119 | `framework/test/unittest/manager/sample_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 120 | `framework/test/unittest/platform/common/adc_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 121 | `framework/test/unittest/platform/common/clock_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 122 | `framework/test/unittest/platform/common/dac_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 123 | `framework/test/unittest/platform/common/emmc_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 124 | `framework/test/unittest/platform/common/gpio_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 125 | `framework/test/unittest/platform/common/hdmi_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 126 | `framework/test/unittest/platform/common/i2c_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 127 | `framework/test/unittest/platform/common/i2s_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 128 | `framework/test/unittest/platform/common/i3c_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 129 | `framework/test/unittest/platform/common/mipi_csi_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 130 | `framework/test/unittest/platform/common/mipi_dsi_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 131 | `framework/test/unittest/platform/common/pcie_bus_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 132 | `framework/test/unittest/platform/common/pcie_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 133 | `framework/test/unittest/platform/common/pin_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 134 | `framework/test/unittest/platform/common/platform_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 135 | `framework/test/unittest/platform/common/pwm_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 136 | `framework/test/unittest/platform/common/regulator_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 137 | `framework/test/unittest/platform/common/rtc_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 138 | `framework/test/unittest/platform/common/sdio_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 139 | `framework/test/unittest/platform/common/spi_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 140 | `framework/test/unittest/platform/common/timer_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 141 | `framework/test/unittest/platform/common/uart_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 142 | `framework/test/unittest/platform/common/watchdog_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 143 | `framework/test/unittest/platform/config/can_test_config.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 144 | `framework/test/unittest/platform/virtual/adc_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 145 | `framework/test/unittest/platform/virtual/clock_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 146 | `framework/test/unittest/platform/virtual/dac_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 147 | `framework/test/unittest/platform/virtual/i3c_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 148 | `framework/test/unittest/platform/virtual/pcie_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 149 | `framework/test/unittest/platform/virtual/pin_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 150 | `framework/test/unittest/platform/virtual/pwm_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 151 | `framework/test/unittest/platform/virtual/regulator_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 152 | `framework/test/unittest/platform/virtual/spi_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 153 | `framework/test/unittest/platform/virtual/watchdog_virtual.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 154 | `framework/test/unittest/pm/hdf_pm_driver_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 155 | `framework/test/unittest/sensor/hdf_sensor_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 156 | `framework/test/unittest/uevent/devmgr_uevent_test.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 157 | `framework/test/unittest/utils/hcs_parser/unittest/hcs_macro_cases.c` | generic tree-sitter ERROR node(s) in preprocessed C++ | 5 |
| 158 | `framework/tools/hdi-gen/ast/ast.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 53 |
| 159 | `framework/tools/hdi-gen/lexer/lexer.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 31 |
| 160 | `framework/tools/hdi-gen/lexer/token.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 10 |
| 161 | `framework/tools/hdi-gen/parser/parser.h` | tree-sitter-cpp node `missing type_identifier` at 4 site(s) | 4 |
| 162 | `interfaces/inner_api/hdi/iservstat_listener_hdi.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 163 | `interfaces/inner_api/utils/hdf_trace.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |

### Per-file details

#### `adapter/khdf/linux/manager/src/hdf_kevent.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 426 | 66 | `missing ;` | `` |
| 426 | 106 | `ERROR` | `=(` |
| 426 | 141 | `ERROR` | `)` |

#### `adapter/khdf/linux/model/camera/src/contig_dma.c`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 36 | 41 | `missing ;` | `` |

#### `adapter/khdf/linux/model/camera/src/sg_dma.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 404 | 45 | `ERROR` | `*` |
| 404 | 68 | `ERROR` | `*` |

#### `adapter/khdf/linux/model/camera/src/virtual_malloc.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 95 | 31 | `ERROR` | `*` |
| 95 | 54 | `ERROR` | `*` |

#### `adapter/khdf/linux/model/storage/emmc_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 162 | 61 | `missing ;` | `` |
| 162 | 101 | `ERROR` | `=(` |
| 162 | 131 | `ERROR` | `)` |

#### `adapter/khdf/linux/model/storage/sdio_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 577 | 61 | `missing ;` | `` |
| 577 | 101 | `ERROR` | `=(` |
| 577 | 131 | `ERROR` | `)` |

#### `adapter/khdf/linux/model/usb/host/src/usb_net_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 1147 | 64 | `missing ;` | `` |
| 1147 | 104 | `ERROR` | `=(` |
| 1147 | 137 | `ERROR` | `)` |

#### `adapter/khdf/linux/model/usb/host/src/usb_pnp_notify.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 795 | 63 | `missing ;` | `` |
| 795 | 103 | `ERROR` | `=(` |
| 795 | 135 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/adc/adc_iio_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 270 | 65 | `missing ;` | `` |
| 270 | 105 | `ERROR` | `=(` |
| 270 | 139 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/clock/clock_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 349 | 67 | `missing ;` | `` |
| 349 | 107 | `ERROR` | `=(` |
| 349 | 143 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/gpio/gpio_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 306 | 66 | `missing ;` | `` |
| 306 | 106 | `ERROR` | `=(` |
| 306 | 141 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/i2c/i2c_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 172 | 65 | `missing ;` | `` |
| 172 | 105 | `ERROR` | `=(` |
| 172 | 139 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/mipi_csi/mipi_v4l2_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 479 | 69 | `missing ;` | `` |
| 479 | 109 | `ERROR` | `=(` |
| 479 | 147 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/mipi_dsi/mipi_drm_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 250 | 69 | `missing ;` | `` |
| 250 | 109 | `ERROR` | `=(` |
| 250 | 147 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/mipi_dsi/mipi_tx_dev.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 453 | 24 | `ERROR` | `struct` |
| 456 | 24 | `ERROR` | `struct` |

#### `adapter/khdf/linux/platform/mipi_dsi/mipi_tx_hi35xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 1233 | 63 | `missing ;` | `` |
| 1233 | 103 | `ERROR` | `=(` |
| 1233 | 135 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/pwm/pwm_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 137 | 52 | `missing ;` | `` |
| 137 | 92 | `ERROR` | `=(` |
| 137 | 113 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/regulator/regulator_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 503 | 71 | `missing ;` | `` |
| 503 | 111 | `ERROR` | `=(` |
| 503 | 151 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/rtc/rtc_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 225 | 60 | `missing ;` | `` |
| 225 | 100 | `ERROR` | `=(` |
| 225 | 129 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/spi/spi_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 529 | 58 | `missing ;` | `` |
| 529 | 98 | `ERROR` | `=(` |
| 529 | 125 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/uart/uart_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 505 | 58 | `missing ;` | `` |
| 505 | 98 | `ERROR` | `=(` |
| 505 | 125 | `ERROR` | `)` |

#### `adapter/khdf/linux/platform/watchdog/watchdog_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 267 | 57 | `missing ;` | `` |
| 267 | 97 | `ERROR` | `=(` |
| 267 | 123 | `ERROR` | `)` |

#### `adapter/khdf/linux/test/platform/i2c/i2c_adapter_dummy.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 106 | 67 | `missing ;` | `` |
| 106 | 107 | `ERROR` | `=(` |
| 106 | 143 | `ERROR` | `)` |

#### `adapter/khdf/liteos/model/storage/src/mmc/mmc_block_lite.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 29 | 32 | `ERROR` | `struct Vnode` |
| 35 | 33 | `ERROR` | `struct Vnode` |
| 41 | 36 | `ERROR` | `struct Vnode` |
| 41 | 71 | `ERROR` | `char` |
| 75 | 37 | `ERROR` | `struct Vnode` |
| 75 | 69 | `ERROR` | `unsigned char` |
| 105 | 36 | `ERROR` | `struct Vnode` |
| 105 | 62 | `ERROR` | `struct geometry` |
| 130 | 44 | `ERROR` | `struct Vnode` |
| 179 | 37 | `ERROR` | `struct Vnode` |
| 199 | 21 | `ERROR` | `struct` |
| 209 | 30 | `ERROR` | `int` |

#### `adapter/khdf/liteos/model/storage/src/mtd/mtd_block_lite.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 5 | 35 | `ERROR` | `struct Vnode` |
| 11 | 36 | `ERROR` | `struct Vnode` |
| 17 | 39 | `ERROR` | `struct Vnode` |
| 17 | 74 | `ERROR` | `char` |
| 27 | 40 | `ERROR` | `struct Vnode` |
| 27 | 72 | `ERROR` | `unsigned char` |
| 37 | 39 | `ERROR` | `struct Vnode` |
| 37 | 65 | `ERROR` | `struct geometry` |
| 44 | 40 | `ERROR` | `struct Vnode` |

#### `adapter/khdf/liteos/model/storage/src/mtd/mtd_char_lite.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 28 | `ERROR` | `struct file` |
| 50 | 29 | `ERROR` | `struct file` |
| 80 | 32 | `ERROR` | `struct file` |
| 80 | 57 | `ERROR` | `char` |
| 152 | 33 | `ERROR` | `struct file` |
| 152 | 64 | `ERROR` | `char` |
| 224 | 31 | `ERROR` | `struct file` |
| 364 | 29 | `ERROR` | `struct file` |
| 381 | 21 | `ERROR` | `struct` |
| 385 | 21 | `ERROR` | `struct` |
| 386 | 22 | `ERROR` | `struct` |
| 416 | 31 | `ERROR` | `struct file` |
| 416 | 56 | `ERROR` | `LosVmMapRegion` |

#### `adapter/khdf/liteos/model/usb/host/src/usb_pnp_manager.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 85 | 64 | `missing ;` | `` |
| 85 | 104 | `ERROR` | `=(` |
| 85 | 137 | `ERROR` | `)` |

#### `adapter/khdf/liteos/model/usb/host/src/usb_pnp_notify.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 883 | 63 | `missing ;` | `` |
| 883 | 103 | `ERROR` | `=(` |
| 883 | 135 | `ERROR` | `)` |

#### `adapter/khdf/liteos/model/usb/host/src/usb_test_pnp_notify.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 59 | 67 | `missing ;` | `` |
| 59 | 107 | `ERROR` | `=(` |
| 59 | 143 | `ERROR` | `)` |

#### `adapter/khdf/liteos_m/test/sample_driver/src/sample_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 58 | 63 | `missing ;` | `` |
| 58 | 103 | `ERROR` | `=(` |
| 58 | 135 | `ERROR` | `)` |

#### `adapter/khdf/uniproton/test/sample_driver/src/platform_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 45 | 63 | `missing ;` | `` |
| 45 | 103 | `ERROR` | `=(` |
| 45 | 135 | `ERROR` | `)` |

#### `adapter/platform/can/can_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 339 | 58 | `missing ;` | `` |
| 339 | 98 | `ERROR` | `=(` |
| 339 | 125 | `ERROR` | `)` |

#### `adapter/platform/gpio/gpio_asr.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 389 | 61 | `missing ;` | `` |
| 389 | 101 | `ERROR` | `=(` |
| 389 | 131 | `ERROR` | `)` |

#### `adapter/platform/gpio/gpio_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 83 | 61 | `missing ;` | `` |
| 83 | 101 | `ERROR` | `=(` |
| 83 | 131 | `ERROR` | `)` |

#### `adapter/platform/gpio/gpio_gr5xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 422 | 61 | `missing ;` | `` |
| 422 | 101 | `ERROR` | `=(` |
| 422 | 131 | `ERROR` | `)` |

#### `adapter/platform/gpio/gpio_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 106 | 61 | `missing ;` | `` |
| 106 | 101 | `ERROR` | `=(` |
| 106 | 131 | `ERROR` | `)` |

#### `adapter/platform/gpio/gpio_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 367 | 61 | `missing ;` | `` |
| 367 | 101 | `ERROR` | `=(` |
| 367 | 131 | `ERROR` | `)` |

#### `adapter/platform/i2c/i2c_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 60 | `missing ;` | `` |
| 20 | 100 | `ERROR` | `=(` |
| 20 | 129 | `ERROR` | `)` |

#### `adapter/platform/i2c/i2c_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 39 | 62 | `missing ;` | `` |
| 39 | 102 | `ERROR` | `=(` |
| 39 | 133 | `ERROR` | `)` |

#### `adapter/platform/i2c/i2c_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 35 | 60 | `missing ;` | `` |
| 35 | 100 | `ERROR` | `=(` |
| 35 | 129 | `ERROR` | `)` |

#### `adapter/platform/mipi_dsi/mipi_drm_imx8mm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 193 | 69 | `missing ;` | `` |
| 193 | 109 | `ERROR` | `=(` |
| 193 | 147 | `ERROR` | `)` |

#### `adapter/platform/pwm/pwm_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 127 | 60 | `missing ;` | `` |
| 127 | 100 | `ERROR` | `=(` |
| 127 | 129 | `ERROR` | `)` |

#### `adapter/platform/pwm/pwm_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 256 | 60 | `missing ;` | `` |
| 256 | 100 | `ERROR` | `=(` |
| 256 | 129 | `ERROR` | `)` |

#### `adapter/platform/pwm/pwm_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 199 | 60 | `missing ;` | `` |
| 199 | 100 | `ERROR` | `=(` |
| 199 | 129 | `ERROR` | `)` |

#### `adapter/platform/spi/spi_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 476 | 60 | `missing ;` | `` |
| 476 | 100 | `ERROR` | `=(` |
| 476 | 129 | `ERROR` | `)` |

#### `adapter/platform/spi/spi_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 478 | 60 | `missing ;` | `` |
| 478 | 100 | `ERROR` | `=(` |
| 478 | 129 | `ERROR` | `)` |

#### `adapter/platform/spi/spi_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 284 | 60 | `missing ;` | `` |
| 284 | 100 | `ERROR` | `=(` |
| 284 | 129 | `ERROR` | `)` |

#### `adapter/platform/uart/uart_asr.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 645 | 59 | `missing ;` | `` |
| 645 | 99 | `ERROR` | `=(` |
| 645 | 127 | `ERROR` | `)` |

#### `adapter/platform/uart/uart_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 323 | 61 | `missing ;` | `` |
| 323 | 101 | `ERROR` | `=(` |
| 323 | 131 | `ERROR` | `)` |

#### `adapter/platform/uart/uart_gr5xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 708 | 59 | `missing ;` | `` |
| 708 | 99 | `ERROR` | `=(` |
| 708 | 127 | `ERROR` | `)` |

#### `adapter/platform/uart/uart_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 271 | 61 | `missing ;` | `` |
| 271 | 101 | `ERROR` | `=(` |
| 271 | 131 | `ERROR` | `)` |

#### `adapter/platform/uart/uart_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 185 | 61 | `missing ;` | `` |
| 185 | 101 | `ERROR` | `=(` |
| 185 | 131 | `ERROR` | `)` |

#### `adapter/platform/watchdog/watchdog_bes.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 127 | 65 | `missing ;` | `` |
| 127 | 105 | `ERROR` | `=(` |
| 127 | 139 | `ERROR` | `)` |

#### `adapter/platform/watchdog/watchdog_stm32f4xx.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 124 | 65 | `missing ;` | `` |
| 124 | 105 | `ERROR` | `=(` |
| 124 | 139 | `ERROR` | `)` |

#### `adapter/platform/watchdog/watchdog_wm.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 254 | 65 | `missing ;` | `` |
| 254 | 105 | `ERROR` | `=(` |
| 254 | 139 | `ERROR` | `)` |

#### `adapter/uhdf2/host/test/unittest/sample1_driver/sample1_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 141 | 64 | `missing ;` | `` |
| 141 | 104 | `ERROR` | `=(` |
| 141 | 137 | `ERROR` | `)` |

#### `adapter/uhdf2/host/test/unittest/sample_driver/sample_driver.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 72 | 63 | `missing ;` | `` |
| 72 | 103 | `ERROR` | `=(` |
| 72 | 111 | `ERROR` | `)(` |
| 72 | 135 | `ERROR` | `)` |

#### `adapter/uhdf2/osal/test/unittest/common/sample_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 128 | 63 | `missing ;` | `` |
| 128 | 103 | `ERROR` | `=(` |
| 128 | 135 | `ERROR` | `)` |

#### `framework/core/manager/test/unittest/common/hdf_sbuf_test.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 276 | 1 | `ERROR` | `ASSERT_EQ(val , static_cast< uint64_t> INT64_MAX)` |

#### `framework/model/audio/core/src/audio_host.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 443 | 62 | `missing ;` | `` |
| 443 | 102 | `ERROR` | `=(` |
| 443 | 133 | `ERROR` | `)` |

#### `framework/model/audio/dispatch/src/audio_control_dispatch.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 632 | 63 | `missing ;` | `` |
| 632 | 103 | `ERROR` | `=(` |
| 632 | 135 | `ERROR` | `)` |

#### `framework/model/audio/dispatch/src/audio_stream_dispatch.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 1678 | 62 | `missing ;` | `` |
| 1678 | 102 | `ERROR` | `=(` |
| 1678 | 133 | `ERROR` | `)` |

#### `framework/model/audio/hdmi/src/audio_hdmi_codec_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 120 | 66 | `missing ;` | `` |
| 120 | 106 | `ERROR` | `=(` |
| 120 | 141 | `ERROR` | `)` |

#### `framework/model/audio/usb/src/audio_usb_codec_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 91 | 65 | `missing ;` | `` |
| 91 | 105 | `ERROR` | `=(` |
| 91 | 139 | `ERROR` | `)` |

#### `framework/model/audio/usb/src/audio_usb_dma_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 120 | 68 | `missing ;` | `` |
| 120 | 108 | `ERROR` | `=(` |
| 120 | 145 | `ERROR` | `)` |

#### `framework/model/audio/usb/src/audio_usb_endpoints.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 289 | 59 | `ERROR` | `struct` |

#### `framework/model/audio/usb/src/audio_usb_mixer.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 104 | 27 | `ERROR` | `, 256` |
| 105 | 27 | `ERROR` | `, 256` |
| 688 | 13 | `ERROR` | `...0x0307` |
| 689 | 13 | `ERROR` | `...0x0607` |
| 690 | 13 | `ERROR` | `...0x0713` |

#### `framework/model/display/driver/adapter_soc/hi35xx_disp.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 311 | 60 | `missing ;` | `` |
| 311 | 100 | `ERROR` | `=(` |
| 311 | 129 | `ERROR` | `)` |

#### `framework/model/display/driver/backlight/hdf_bl.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 427 | 56 | `missing ;` | `` |
| 427 | 96 | `ERROR` | `=(` |
| 427 | 121 | `ERROR` | `)` |

#### `framework/model/display/driver/backlight/pwm_bl.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 162 | 59 | `missing ;` | `` |
| 162 | 99 | `ERROR` | `=(` |
| 162 | 127 | `ERROR` | `)` |

#### `framework/model/display/driver/hdf_disp.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 673 | 58 | `missing ;` | `` |
| 673 | 98 | `ERROR` | `=(` |
| 673 | 125 | `ERROR` | `)` |

#### `framework/model/display/driver/hdf_drm_panel.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 135 | 20 | `ERROR` | `suspend , S_IWUSR , NULL ,` |
| 135 | 60 | `missing identifier` | `` |
| 155 | 20 | `ERROR` | `resume , S_IWUSR , NULL ,` |
| 155 | 58 | `missing identifier` | `` |
| 180 | 20 | `ERROR` | `backlight , S_IWUSR , NULL ,` |
| 180 | 64 | `missing identifier` | `` |
| 275 | 62 | `missing ;` | `` |
| 275 | 102 | `ERROR` | `=(` |
| 275 | 133 | `ERROR` | `)` |

#### `framework/model/display/driver/lcdkit/lite_lcdkit.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 282 | 60 | `missing ;` | `` |
| 282 | 100 | `ERROR` | `=(` |
| 282 | 129 | `ERROR` | `)` |

#### `framework/model/display/driver/panel/ili9881_st_5p5.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 675 | 57 | `missing ;` | `` |
| 675 | 97 | `ERROR` | `=(` |
| 675 | 123 | `ERROR` | `)` |

#### `framework/model/display/driver/panel/ili9881c_boe.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 675 | 65 | `missing ;` | `` |
| 675 | 105 | `ERROR` | `=(` |
| 675 | 139 | `ERROR` | `)` |

#### `framework/model/display/driver/panel/mipi_icn9700.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 299 | 61 | `missing ;` | `` |
| 299 | 101 | `ERROR` | `=(` |
| 299 | 131 | `ERROR` | `)` |

#### `framework/model/display/driver/panel/ssp_st7789.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 266 | 60 | `missing ;` | `` |
| 266 | 100 | `ERROR` | `=(` |
| 266 | 129 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_encoder.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 293 | 61 | `missing ;` | `` |
| 293 | 101 | `ERROR` | `=(` |
| 293 | 131 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_encoder.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 15 | 25 | `missing ;` | `` |

#### `framework/model/input/driver/hdf_hid_adapter.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 451 | 57 | `missing ;` | `` |
| 451 | 97 | `ERROR` | `=(` |
| 451 | 123 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_infrared.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 361 | 62 | `missing ;` | `` |
| 361 | 102 | `ERROR` | `=(` |
| 361 | 133 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_input_device_manager.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 495 | 59 | `missing ;` | `` |
| 495 | 99 | `ERROR` | `=(` |
| 495 | 127 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_key.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 220 | 57 | `missing ;` | `` |
| 220 | 97 | `ERROR` | `=(` |
| 220 | 123 | `ERROR` | `)` |

#### `framework/model/input/driver/hdf_touch.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 854 | 59 | `missing ;` | `` |
| 854 | 99 | `ERROR` | `=(` |
| 854 | 127 | `ERROR` | `)` |

#### `framework/model/input/driver/touchscreen/touch_ft5406.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 359 | 65 | `missing ;` | `` |
| 359 | 105 | `ERROR` | `=(` |
| 359 | 139 | `ERROR` | `)` |

#### `framework/model/input/driver/touchscreen/touch_ft5x06.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 396 | 68 | `missing ;` | `` |
| 396 | 108 | `ERROR` | `=(` |
| 396 | 145 | `ERROR` | `)` |

#### `framework/model/input/driver/touchscreen/touch_ft6336.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 206 | 65 | `missing ;` | `` |
| 206 | 105 | `ERROR` | `=(` |
| 206 | 139 | `ERROR` | `)` |

#### `framework/model/input/driver/touchscreen/touch_gt911.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 334 | 66 | `missing ;` | `` |
| 334 | 106 | `ERROR` | `=(` |
| 334 | 141 | `ERROR` | `)` |

#### `framework/model/misc/dsoftbus/src/hdf_dsoftbus_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 63 | 61 | `missing ;` | `` |
| 63 | 101 | `ERROR` | `=(` |
| 63 | 131 | `ERROR` | `)` |

#### `framework/model/misc/light/driver/src/light_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 624 | 62 | `missing ;` | `` |
| 624 | 102 | `ERROR` | `=(` |
| 624 | 133 | `ERROR` | `)` |

#### `framework/model/misc/vibrator/driver/src/vibrator_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 441 | 65 | `missing ;` | `` |
| 441 | 105 | `ERROR` | `=(` |
| 441 | 139 | `ERROR` | `)` |

#### `framework/model/network/ethernet/src/hdf_eth_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 206 | 54 | `missing ;` | `` |
| 206 | 94 | `ERROR` | `=(` |
| 206 | 117 | `ERROR` | `)` |

#### `framework/model/network/wifi/core/hdf_wifi_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 546 | 58 | `missing ;` | `` |
| 546 | 98 | `ERROR` | `=(` |
| 546 | 125 | `ERROR` | `)` |

#### `framework/model/sensor/driver/accel/sensor_accel_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 395 | 65 | `missing ;` | `` |
| 395 | 105 | `ERROR` | `=(` |
| 395 | 139 | `ERROR` | `)` |

#### `framework/model/sensor/driver/accel/sensor_gravity_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 322 | 67 | `missing ;` | `` |
| 322 | 107 | `ERROR` | `=(` |
| 322 | 143 | `ERROR` | `)` |

#### `framework/model/sensor/driver/als/sensor_als_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 516 | 63 | `missing ;` | `` |
| 516 | 103 | `ERROR` | `=(` |
| 516 | 135 | `ERROR` | `)` |

#### `framework/model/sensor/driver/barometer/sensor_barometer_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 362 | 69 | `missing ;` | `` |
| 362 | 109 | `ERROR` | `=(` |
| 362 | 147 | `ERROR` | `)` |

#### `framework/model/sensor/driver/common/src/sensor_device_manager.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 406 | 67 | `missing ;` | `` |
| 406 | 107 | `ERROR` | `=(` |
| 406 | 143 | `ERROR` | `)` |

#### `framework/model/sensor/driver/gas/sensor_gas_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 382 | 63 | `missing ;` | `` |
| 382 | 103 | `ERROR` | `=(` |
| 382 | 135 | `ERROR` | `)` |

#### `framework/model/sensor/driver/gyro/sensor_gyro_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 372 | 64 | `missing ;` | `` |
| 372 | 104 | `ERROR` | `=(` |
| 372 | 137 | `ERROR` | `)` |

#### `framework/model/sensor/driver/hall/sensor_hall_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 478 | 64 | `missing ;` | `` |
| 478 | 104 | `ERROR` | `=(` |
| 478 | 137 | `ERROR` | `)` |

#### `framework/model/sensor/driver/humidity/sensor_humidity_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 372 | 68 | `missing ;` | `` |
| 372 | 108 | `ERROR` | `=(` |
| 372 | 145 | `ERROR` | `)` |

#### `framework/model/sensor/driver/magnetic/sensor_magnetic_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 370 | 68 | `missing ;` | `` |
| 370 | 108 | `ERROR` | `=(` |
| 370 | 145 | `ERROR` | `)` |

#### `framework/model/sensor/driver/pedometer/sensor_pedometer_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 363 | 69 | `missing ;` | `` |
| 363 | 109 | `ERROR` | `=(` |
| 363 | 147 | `ERROR` | `)` |

#### `framework/model/sensor/driver/ppg/sensor_ppg_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 321 | 63 | `missing ;` | `` |
| 321 | 103 | `ERROR` | `=(` |
| 321 | 135 | `ERROR` | `)` |

#### `framework/model/sensor/driver/proximity/sensor_proximity_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 362 | 69 | `missing ;` | `` |
| 362 | 109 | `ERROR` | `=(` |
| 362 | 147 | `ERROR` | `)` |

#### `framework/model/sensor/driver/temperature/sensor_temperature_driver.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 372 | 71 | `missing ;` | `` |
| 372 | 111 | `ERROR` | `=(` |
| 372 | 151 | `ERROR` | `)` |

#### `framework/sample/platform/uart/src/uart_sample.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 22 | 67 | `missing ;` | `` |
| 22 | 107 | `ERROR` | `=(` |
| 22 | 143 | `ERROR` | `)` |

#### `framework/support/platform/src/adc/adc_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 494 | 61 | `missing ;` | `` |
| 494 | 101 | `ERROR` | `=(` |
| 494 | 131 | `ERROR` | `)` |

#### `framework/support/platform/src/clock/clock_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 849 | 63 | `missing ;` | `` |
| 849 | 103 | `ERROR` | `=(` |
| 849 | 135 | `ERROR` | `)` |

#### `framework/support/platform/src/dac/dac_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 505 | 61 | `missing ;` | `` |
| 505 | 101 | `ERROR` | `=(` |
| 505 | 131 | `ERROR` | `)` |

#### `framework/support/platform/src/gpio/gpio_service.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 397 | 62 | `missing ;` | `` |
| 397 | 102 | `ERROR` | `=(` |
| 397 | 133 | `ERROR` | `)` |

#### `framework/support/platform/src/i2c/i2c_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 269 | 61 | `missing ;` | `` |
| 269 | 101 | `ERROR` | `=(` |
| 269 | 131 | `ERROR` | `)` |

#### `framework/support/platform/src/i3c/i3c_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 755 | 61 | `missing ;` | `` |
| 755 | 101 | `ERROR` | `=(` |
| 755 | 131 | `ERROR` | `)` |

#### `framework/support/platform/src/pin/pin_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 740 | 61 | `missing ;` | `` |
| 740 | 101 | `ERROR` | `=(` |
| 740 | 131 | `ERROR` | `)` |

#### `framework/support/platform/src/regulator/regulator_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 728 | 67 | `missing ;` | `` |
| 728 | 107 | `ERROR` | `=(` |
| 728 | 143 | `ERROR` | `)` |

#### `framework/support/platform/src/timer/timer_core.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 604 | 63 | `missing ;` | `` |
| 604 | 103 | `ERROR` | `=(` |
| 604 | 135 | `ERROR` | `)` |

#### `framework/test/unittest/common/hdf_main_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 108 | 59 | `missing ;` | `` |
| 108 | 99 | `ERROR` | `=(` |
| 108 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/manager/sample_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 284 | 63 | `missing ;` | `` |
| 284 | 103 | `ERROR` | `=(` |
| 284 | 135 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/adc_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 117 | 58 | `missing ;` | `` |
| 117 | 98 | `ERROR` | `=(` |
| 117 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/clock_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 94 | 60 | `missing ;` | `` |
| 94 | 100 | `ERROR` | `=(` |
| 94 | 129 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/dac_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 118 | 58 | `missing ;` | `` |
| 118 | 98 | `ERROR` | `=(` |
| 118 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/emmc_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 156 | 59 | `missing ;` | `` |
| 156 | 99 | `ERROR` | `=(` |
| 156 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/gpio_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 152 | 59 | `missing ;` | `` |
| 152 | 99 | `ERROR` | `=(` |
| 152 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/hdmi_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 287 | 59 | `missing ;` | `` |
| 287 | 99 | `ERROR` | `=(` |
| 287 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/i2c_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 116 | 58 | `missing ;` | `` |
| 116 | 98 | `ERROR` | `=(` |
| 116 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/i2s_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 498 | 58 | `missing ;` | `` |
| 498 | 98 | `ERROR` | `=(` |
| 498 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/i3c_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 117 | 58 | `missing ;` | `` |
| 117 | 98 | `ERROR` | `=(` |
| 117 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/mipi_csi_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 489 | 62 | `missing ;` | `` |
| 489 | 102 | `ERROR` | `=(` |
| 489 | 133 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/mipi_dsi_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 200 | 62 | `missing ;` | `` |
| 200 | 102 | `ERROR` | `=(` |
| 200 | 133 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/pcie_bus_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 94 | 62 | `missing ;` | `` |
| 94 | 102 | `ERROR` | `=(` |
| 94 | 133 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/pcie_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 91 | 59 | `missing ;` | `` |
| 91 | 99 | `ERROR` | `=(` |
| 91 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/pin_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 114 | 58 | `missing ;` | `` |
| 114 | 98 | `ERROR` | `=(` |
| 114 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/platform_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 44 | 63 | `missing ;` | `` |
| 44 | 103 | `ERROR` | `=(` |
| 44 | 135 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/pwm_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 123 | 58 | `missing ;` | `` |
| 123 | 98 | `ERROR` | `=(` |
| 123 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/regulator_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 409 | 64 | `missing ;` | `` |
| 409 | 104 | `ERROR` | `=(` |
| 409 | 137 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/rtc_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 149 | 58 | `missing ;` | `` |
| 149 | 98 | `ERROR` | `=(` |
| 149 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/sdio_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 449 | 59 | `missing ;` | `` |
| 449 | 99 | `ERROR` | `=(` |
| 449 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/spi_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 131 | 58 | `missing ;` | `` |
| 131 | 98 | `ERROR` | `=(` |
| 131 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/timer_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 109 | 60 | `missing ;` | `` |
| 109 | 100 | `ERROR` | `=(` |
| 109 | 129 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/uart_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 126 | 59 | `missing ;` | `` |
| 126 | 99 | `ERROR` | `=(` |
| 126 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/platform/common/watchdog_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 104 | 63 | `missing ;` | `` |
| 104 | 103 | `ERROR` | `=(` |
| 104 | 135 | `ERROR` | `)` |

#### `framework/test/unittest/platform/config/can_test_config.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 112 | 58 | `missing ;` | `` |
| 112 | 98 | `ERROR` | `=(` |
| 112 | 125 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/adc_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 169 | 67 | `missing ;` | `` |
| 169 | 107 | `ERROR` | `=(` |
| 169 | 143 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/clock_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 253 | 69 | `missing ;` | `` |
| 253 | 109 | `ERROR` | `=(` |
| 253 | 147 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/dac_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 176 | 60 | `missing ;` | `` |
| 176 | 100 | `ERROR` | `=(` |
| 176 | 129 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/i3c_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 427 | 60 | `missing ;` | `` |
| 427 | 100 | `ERROR` | `=(` |
| 427 | 129 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/pcie_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 206 | 68 | `missing ;` | `` |
| 206 | 108 | `ERROR` | `=(` |
| 206 | 145 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/pin_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 385 | 67 | `missing ;` | `` |
| 385 | 107 | `ERROR` | `=(` |
| 385 | 143 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/pwm_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 147 | 56 | `missing ;` | `` |
| 147 | 96 | `ERROR` | `=(` |
| 147 | 121 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/regulator_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 271 | 66 | `missing ;` | `` |
| 271 | 106 | `ERROR` | `=(` |
| 271 | 141 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/spi_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 455 | 62 | `missing ;` | `` |
| 455 | 102 | `ERROR` | `=(` |
| 455 | 133 | `ERROR` | `)` |

#### `framework/test/unittest/platform/virtual/watchdog_virtual.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 169 | 72 | `missing ;` | `` |
| 169 | 112 | `ERROR` | `=(` |
| 169 | 153 | `ERROR` | `)` |

#### `framework/test/unittest/pm/hdf_pm_driver_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 600 | 59 | `missing ;` | `` |
| 600 | 99 | `ERROR` | `=(` |
| 600 | 127 | `ERROR` | `)` |

#### `framework/test/unittest/sensor/hdf_sensor_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 248 | 64 | `missing ;` | `` |
| 248 | 104 | `ERROR` | `=(` |
| 248 | 137 | `ERROR` | `)` |

#### `framework/test/unittest/uevent/devmgr_uevent_test.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 60 | 63 | `missing ;` | `` |
| 60 | 103 | `ERROR` | `=(` |
| 60 | 135 | `ERROR` | `)` |

#### `framework/test/unittest/utils/hcs_parser/unittest/hcs_macro_cases.c`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 139 | 1 | `ERROR` | `do{if(strcmp(FP_INFO_NODE_ , g_fpData[index] .product)){do{char tmp_fmt[256]={0 } ;if(DealFormat("[" "E" "/" "HDF_LOG_TA…` |
| 164 | 1 | `ERROR` | `do{if(strcmp(FP_INFO_NODE_ ,(g_fpData)[(index)] .product)){do{char tmp_fmt[256]={0 } ;if(DealFormat("[" "E" "/" "HDF_LOG…` |
| 173 | 1 | `ERROR` | `do{if(strcmp(FP_INFO_NODE_ ,(g_fpData)[(index)] .product)){do{char tmp_fmt[256]={0 } ;if(DealFormat("[" "E" "/" "HDF_LOG…` |
| 182 | 1 | `ERROR` | `do{if(strcmp(HCS_ROOT_ ,(g_fpData)[(index)] .product)){do{char tmp_fmt[256]={0 } ;if(DealFormat("[" "E" "/" "HDF_LOG_TAG…` |
| 315 | 2 | `missing }` | `` |

#### `framework/tools/hdi-gen/ast/ast.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 13 | 18 | `ERROR` | `, }` |
| 16 | 7 | `ERROR` | `AST: public` |
| 18 | 18 | `ERROR` | `std:: unordered_map< std::` |
| 18 | 61 | `ERROR` | `< AST>>` |
| 19 | 22 | `ERROR` | `std:: unordered_map< std::` |
| 19 | 58 | `ERROR` | `AutoPtr<` |
| 19 | 74 | `ERROR` | `>>` |
| 21 | 3 | `ERROR` | `AST()` |
| 28 | 1 | `ERROR` | `ASTFileType GetASTFileType() const` |
| 33 | 26 | `ERROR` | `:: string&` |
| 35 | 11 | `ERROR` | `:: string` |
| 40 | 27 | `ERROR` | `:: string&` |
| 42 | 11 | `ERROR` | `:: string` |
| 47 | 11 | `ERROR` | `:: string` |
| 52 | 33 | `ERROR` | `:: string&` |
| 57 | 11 | `ERROR` | `:: string` |
| 62 | 30 | `ERROR` | `:: string&` |
| 64 | 39 | `ERROR` | `const std::` |
| 66 | 32 | `ERROR` | `< ASTNamespace> &` |
| 68 | 38 | `ERROR` | `const std::` |
| … | … | … | *(33 more)* |

#### `framework/tools/hdi-gen/lexer/lexer.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 0 | 0 | `?` | `line 26 col 33 (missing )) ` |
| 0 | 0 | `?` | `line 28 col 32 (missing )) ` |
| 0 | 0 | `?` | `line 48 col 47 (missing )) ` |
| 10 | 12 | `ERROR` | `ParseMode{` |
| 13 | 1 | `ERROR` | `} ;` |
| 19 | 21 | `ERROR` | `:: string&` |
| 21 | 11 | `ERROR` | `:: string` |
| 26 | 39 | `ERROR` | `)` |
| 28 | 38 | `ERROR` | `)` |
| 48 | 21 | `ERROR` | `&` |
| 48 | 53 | `ERROR` | `)` |
| 50 | 24 | `ERROR` | `&` |
| 52 | 18 | `ERROR` | `&` |
| 54 | 19 | `ERROR` | `&` |
| 56 | 25 | `ERROR` | `&` |
| 58 | 22 | `ERROR` | `&` |
| 60 | 22 | `ERROR` | `&` |
| 62 | 22 | `ERROR` | `&` |
| 64 | 25 | `ERROR` | `&` |
| 66 | 27 | `ERROR` | `&` |
| … | … | … | *(11 more)* |

#### `framework/tools/hdi-gen/lexer/token.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 78 | 13 | `ERROR` | `, }` |
| 82 | 4 | `ERROR` | `:: string` |
| 90 | 4 | `ERROR` | `:: string` |
| 92 | 4 | `ERROR` | `:: string` |
| 96 | 28 | `ERROR` | `&` |
| 96 | 47 | `ERROR` | `&` |
| 100 | 1 | `ERROR` | `}` |
| 102 | 5 | `ERROR` | `:` |
| 102 | 33 | `ERROR` | `&` |
| 104 | 2 | `missing }` | `` |

#### `framework/tools/hdi-gen/parser/parser.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 4 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 67 | 42 | `missing type_identifier` | `` |
| 107 | 48 | `missing type_identifier` | `` |
| 114 | 50 | `missing type_identifier` | `` |
| 121 | 49 | `missing type_identifier` | `` |

#### `interfaces/inner_api/hdi/iservstat_listener_hdi.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 41 | 31 | `ERROR` | `"HDI.IServiceStatusListener.V1_0"` |

#### `interfaces/inner_api/utils/hdf_trace.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 7 | 17 | `ERROR` | `const std:: string& value , const std::` |
| 12 | 1 | `ERROR` | `{` |
| 15 | 7 | `ERROR` | `~` |

---

## hiviewdfx_hiview

Generated from `trace analyze /private/tmp/corpora/hiviewdfx_hiview` (32 files with parse warnings).
Each entry is a translation unit or header indexed as its own file; reasons come from tree-sitter ERROR sites in preprocessed source.

**Total failing files:** 32

### Failure categories

| Category | Files |
|----------|------:|
| generic ERROR nodes (mixed C++ constructs) | 19 |
| missing type identifiers (often macro-expanded types) | 7 |
| other / mixed | 3 |
| extern template instantiations | 2 |
| gtest/HWTEST macros (`missing ;`) | 1 |

### File list

| # | File | Reason | Error sites |
|---|------|--------|-------------|
| 1 | `adapter/plugins/eventservice/service/idl/dfx/include/listener_status_util.h` | tree-sitter-cpp node `missing type_identifier` at 2 site(s) | 2 |
| 2 | `adapter/plugins/eventservice/service/idl/include/iquery_sys_event_callback.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 3 | `adapter/plugins/eventservice/service/idl/include/parcelable_vector_rw.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 9 |
| 4 | `base/event_loop.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 5 | `base/event_raw/encoded/encoded_param.cpp` | tree-sitter-cpp node `missing identifier` at 3 site(s) | 3 |
| 6 | `base/event_raw/include/encoded/encoded_param.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 9 |
| 7 | `base/event_raw/include/encoded/raw_data_builder.h` | explicit template instantiation declarations (`extern template …`) not supported by tree-sitter-cpp | 10 |
| 8 | `base/event_store/include/sys_event_query.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 9 | `base/include/event.h` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |
| 10 | `base/include/plugin.h` | tree-sitter-cpp node `missing type_identifier` at 3 site(s) | 3 |
| 11 | `base/include/sys_event.h` | explicit template instantiation declarations (`extern template …`) not supported by tree-sitter-cpp | 8 |
| 12 | `core/param_update/include/param_event_manager.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 13 | `framework/native/unified_collection/collector/impl/cpu/device_client/collect_device_client.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 12 |
| 14 | `framework/native/unified_collection/collector/impl/trace/strategy/include/trace_handler.h` | tree-sitter-cpp node `missing type_identifier` at 2 site(s) | 2 |
| 15 | `framework/native/unified_collection/trace_manager/test/trace_manager_test.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 16 | `hiretrieval/frameworks/include/hiretrieval_base_def.h` | tree-sitter-cpp node `missing identifier` at 1 site(s) | 1 |
| 17 | `hiretrieval/interfaces/ets/ani/src/hiretrieval_ani.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 18 | `hiretrieval/interfaces/js/napi/src/hiretrieval_napi.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 19 | `interfaces/ets/ani/loglibrary/src/loglibrary_ani.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 20 | `interfaces/inner_api/unified_collection/client/src/trace_collector_client_impl.cpp` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |
| 21 | `interfaces/inner_api/unified_collection/client/trace_collector_client.h` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |
| 22 | `interfaces/js/napi/src/napi_hiview_js.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 23 | `plugins/eventlogger/log_catcher/summary_log_info_catcher.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 24 | `plugins/faultlogger/interfaces/cj/faultlogger_ffi.h` | tree-sitter-cpp node `missing ::` at 1 site(s) | 1 |
| 25 | `plugins/faultlogger/interfaces/js/napi/napi_faultlogger.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 26 | `plugins/faultlogger/interfaces/js/test/unittest/cpp/faultlogger_test_napi.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 27 | `plugins/faultlogger/service/idl/include/ifaultlog_query_result.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 28 | `plugins/faultlogger/service/idl/include/ifaultlogger_service.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 29 | `plugins/performance/perfmonitor/common/event_builder/xperf_event_builder.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 30 | `plugins/performance/perfmonitor/interfaces/inner_api/include/perf_model.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 31 | `plugins/performance/xperf_service/services/utils/time_util.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 5 |
| 32 | `plugins/usage_event_report/fold/cache/include/fold_app_usage_db_helper.h` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |

### Per-file details

#### `adapter/plugins/eventservice/service/idl/dfx/include/listener_status_util.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 2 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 10 | 91 | `missing type_identifier` | `` |
| 12 | 43 | `missing type_identifier` | `` |

#### `adapter/plugins/eventservice/service/idl/include/iquery_sys_event_callback.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 15 | 31 | `ERROR` | `"ohos.hiviewdfx.IQuerySysEventCallback"` |

#### `adapter/plugins/eventservice/service/idl/include/parcelable_vector_rw.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 14 | 20 | `ERROR` | `T` |
| 15 | 1 | `ERROR` | `bool` |
| 15 | 44 | `ERROR` | `std::` |
| 15 | 69 | `missing ;` | `` |
| 28 | 40 | `missing identifier` | `` |
| 40 | 20 | `ERROR` | `T` |
| 41 | 1 | `ERROR` | `bool` |
| 41 | 43 | `ERROR` | `const std::` |
| 41 | 74 | `missing ;` | `` |

#### `base/event_loop.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 310 | 20 | `ERROR` | `-> operator()` |

#### `base/event_raw/encoded/encoded_param.cpp`

**Summary:** tree-sitter-cpp node `missing identifier` at 3 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 51 | `missing identifier` | `` |
| 10 | 50 | `missing identifier` | `` |
| 11 | 53 | `missing identifier` | `` |

#### `base/event_raw/include/encoded/encoded_param.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 42 | 98 | `ERROR` | `*` |
| 83 | 98 | `ERROR` | `*` |
| 132 | 48 | `ERROR` | `*` |
| 174 | 48 | `ERROR` | `*` |
| 221 | 112 | `ERROR` | `*` |
| 268 | 112 | `ERROR` | `*` |
| 396 | 17 | `ERROR` | `class` |
| 397 | 17 | `ERROR` | `class` |
| 398 | 17 | `ERROR` | `class` |

#### `base/event_raw/include/encoded/raw_data_builder.h`

**Summary:** explicit template instantiation declarations (`extern template …`) not supported by tree-sitter-cpp

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 514 | 8 | `ERROR` | `template void RawDataBuilder` |
| 515 | 84 | `ERROR` | `&` |
| 516 | 8 | `ERROR` | `template void RawDataBuilder` |
| 517 | 85 | `ERROR` | `&` |
| 518 | 8 | `ERROR` | `template void RawDataBuilder` |
| 519 | 81 | `ERROR` | `&` |
| 520 | 8 | `ERROR` | `template RawDataBuilder& RawDataBuilder:: AppendValue< double>(const std:: string& , double) ;` |
| 521 | 8 | `ERROR` | `template RawDataBuilder& RawDataBuilder:: AppendValue< std:: string>(const std:: string& , std:: string) ;` |
| 522 | 8 | `ERROR` | `template RawDataBuilder& RawDataBuilder:: AppendValue< int64_t>(const std:: string& , int64_t) ;` |
| 523 | 8 | `ERROR` | `template RawDataBuilder` |

#### `base/event_store/include/sys_event_query.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 89 | 48 | `ERROR` | `*` |
| 271 | 59 | `missing type_identifier` | `` |

#### `base/include/event.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 244 | 62 | `missing type_identifier` | `` |

#### `base/include/plugin.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 3 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 74 | 55 | `missing type_identifier` | `` |
| 74 | 109 | `missing type_identifier` | `` |
| 75 | 72 | `missing type_identifier` | `` |

#### `base/include/sys_event.h`

**Summary:** explicit template instantiation declarations (`extern template …`) not supported by tree-sitter-cpp

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 0 | 0 | `?` | `line 165 col 96 (missing )) ` |
| 165 | 8 | `ERROR` | `template void SysEvent` |
| 165 | 68 | `ERROR` | `std` |
| 165 | 80 | `ERROR` | `& , std` |
| 165 | 103 | `ERROR` | `)` |
| 166 | 8 | `ERROR` | `template void SysEvent` |
| 166 | 64 | `ERROR` | `std` |
| 166 | 76 | `ERROR` | `& , uint64_t , bool` |

#### `core/param_update/include/param_event_manager.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 37 | `missing ;` | `` |

#### `framework/native/unified_collection/collector/impl/cpu/device_client/collect_device_client.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 56 | 61 | `ERROR` | `int` |
| 68 | 104 | `ERROR` | `struct` |
| 70 | 50 | `ERROR` | `struct` |
| 81 | 104 | `ERROR` | `struct` |
| 83 | 50 | `ERROR` | `struct` |
| 95 | 54 | `ERROR` | `struct` |
| 107 | 58 | `ERROR` | `struct` |
| 123 | 76 | `ERROR` | `struct` |
| 124 | 52 | `ERROR` | `struct` |
| 134 | 180 | `ERROR` | `struct` |
| 140 | 76 | `ERROR` | `struct` |
| 141 | 52 | `ERROR` | `struct` |

#### `framework/native/unified_collection/collector/impl/trace/strategy/include/trace_handler.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 2 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 21 | 100 | `missing type_identifier` | `` |
| 35 | 92 | `missing type_identifier` | `` |

#### `framework/native/unified_collection/trace_manager/test/trace_manager_test.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 57 | 1 | `ERROR` | `.args` |
| 65 | 1 | `ERROR` | `.args` |
| 73 | 1 | `ERROR` | `.args` |

#### `hiretrieval/frameworks/include/hiretrieval_base_def.h`

**Summary:** tree-sitter-cpp node `missing identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 2 | `missing identifier` | `` |

#### `hiretrieval/interfaces/ets/ani/src/hiretrieval_ani.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 59 | 12 | `ERROR` | `ani_status` |

#### `hiretrieval/interfaces/js/napi/src/hiretrieval_napi.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 89 | 8 | `ERROR` | `napi_value` |
| 106 | 20 | `ERROR` | `_module` |

#### `interfaces/ets/ani/loglibrary/src/loglibrary_ani.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 67 | 12 | `ERROR` | `ani_status` |

#### `interfaces/inner_api/unified_collection/client/src/trace_collector_client_impl.cpp`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 16 | 43 | `missing type_identifier` | `` |

#### `interfaces/inner_api/unified_collection/client/trace_collector_client.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 40 | 43 | `missing type_identifier` | `` |

#### `interfaces/js/napi/src/napi_hiview_js.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 140 | 8 | `ERROR` | `napi_value` |
| 153 | 20 | `ERROR` | `_module` |

#### `plugins/eventlogger/log_catcher/summary_log_info_catcher.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 58 | 45 | `ERROR` | `, int32_t` |

#### `plugins/faultlogger/interfaces/cj/faultlogger_ffi.h`

**Summary:** tree-sitter-cpp node `missing ::` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 26 | 28 | `missing ::` | `` |

#### `plugins/faultlogger/interfaces/js/napi/napi_faultlogger.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 393 | 1 | `ERROR` | `napi_value` |
| 407 | 19 | `missing ::` | `` |

#### `plugins/faultlogger/interfaces/js/test/unittest/cpp/faultlogger_test_napi.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 8 | `ERROR` | `napi_value` |
| 31 | 20 | `ERROR` | `demoModule` |

#### `plugins/faultlogger/service/idl/include/ifaultlog_query_result.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 14 | 31 | `ERROR` | `"ohos.hiviewdfx.IFaultLogQueryResult"` |

#### `plugins/faultlogger/service/idl/include/ifaultlogger_service.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 31 | `ERROR` | `"ohos.hiviewdfx.IFaultLoggerService"` |

#### `plugins/performance/perfmonitor/common/event_builder/xperf_event_builder.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 240 | 10 | `ERROR` | `*` |

#### `plugins/performance/perfmonitor/interfaces/inner_api/include/perf_model.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 244 | 31 | `ERROR` | `"OHOS.HiviewDFX.IFrameCallback"` |
| 253 | 31 | `ERROR` | `"OHOS.HiviewDFX.IAnimatorCallback"` |
| 262 | 31 | `ERROR` | `"OHOS.HiviewDFX.ISceneCallback"` |

#### `plugins/performance/xperf_service/services/utils/time_util.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 1 | `ERROR` | `static int64_t GetCurrTimeMs()` |
| 11 | 20 | `ERROR` | `:: duration_cast< std:` |
| 11 | 51 | `ERROR` | `:` |
| 12 | 1 | `ERROR` | `std:: chrono:: system_clock::` |
| 16 | 1 | `ERROR` | `}` |

#### `plugins/usage_event_report/fold/cache/include/fold_app_usage_db_helper.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 92 | 101 | `missing type_identifier` | `` |

---

## multimedia_camera_framework

Generated from `trace analyze /private/tmp/corpora/multimedia_camera_framework` (64 files with parse warnings).
Each entry is a translation unit or header indexed as its own file; reasons come from tree-sitter ERROR sites in preprocessed source.

**Total failing files:** 64

### Failure categories

| Category | Files |
|----------|------:|
| generic ERROR nodes (mixed C++ constructs) | 43 |
| gtest/HWTEST macros (`missing ;`) | 18 |
| missing type identifiers (often macro-expanded types) | 2 |
| other / mixed | 1 |

### File list

| # | File | Reason | Error sites |
|---|------|--------|-------------|
| 1 | `common/utils/media_capability_interface.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 5 |
| 2 | `dynamic_libs/media_manager/include/media_manager/mpeg_manager_factory.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 3 | `dynamic_libs/media_manager/include/media_manager/track_factory.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 4 | `dynamic_libs/moving_photo/src/moving_photo_adapter.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 10 |
| 5 | `frameworks/cj/camera/include/camera_ffi.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 164 |
| 6 | `frameworks/cj/camera/include/camera_input_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 7 | `frameworks/cj/camera/include/camera_manager_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 8 | `frameworks/cj/camera/include/camera_output_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 9 | `frameworks/cj/camera/include/camera_session_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 10 | `frameworks/cj/camera/include/metadata_output_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 11 | `frameworks/cj/camera/include/photo_output_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 12 | `frameworks/cj/camera/include/preview_output_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 13 | `frameworks/cj/camera/include/video_output_impl.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 14 | `frameworks/cj/camera/src/camera_ffi.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 42 |
| 15 | `frameworks/cj/camera_picker/include/camera_picker_ffi.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 16 | `frameworks/native/camera/base/src/ability/camera_ability_builder.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 17 | `frameworks/native/camera/test/ndktest/camera_ndk_demo/entry/src/main/cpp/main.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 18 | `frameworks/native/camera/test/unittest/camera_deferred_unittest/camera_deferred_schedule_test/include/camera_deferred_video_unittest.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 17 |
| 19 | `frameworks/native/camera/test/unittest/camera_deferred_unittest/camera_deferred_session_test/src/deferred_photo_session_unittest.cpp` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 20 | `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/include/hcamera_device_unittest.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 21 | `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/include/hcamera_service_unittest.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 11 |
| 22 | `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/src/hcamera_device_manager_unittest.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 23 | `frameworks/native/camera/test/unittest/camera_service/hdi_stream_test/include/hstream_operator_unittest.h` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |
| 24 | `frameworks/native/camera/test/unittest/camera_service/hdi_stream_test/src/hstream_capture_unittest.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 25 | `frameworks/native/camera/test/unittest/framework_native/output/src/photo_output_unittest.cpp` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 26 | `frameworks/native/camera/test/unittest/framework_native/session/src/capture_session_unittest.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 27 | `frameworks/native/ndk/impl/camera_manager_impl.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 8 |
| 28 | `frameworks/native/ndk/impl/metadata_output_impl.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 29 | `frameworks/taihe/include/camera_event_emitter_taihe.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 4 |
| 30 | `frameworks/taihe/src/camera_constructor_taihe.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 31 | `frameworks/taihe/src/camera_picker_constructor_taihe.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 32 | `interfaces/inner_api/native/camera/include/ability/camera_ability_builder.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 33 | `interfaces/inner_api/native/camera/include/input/camera_manager.h` | tree-sitter-cpp node `missing type_identifier` at 1 site(s) | 1 |
| 34 | `interfaces/inner_api/native/camera/include/input/i_standard_camera_listener.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 35 | `interfaces/inner_api/native/camera/include/output/photo_output.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 36 | `interfaces/kits/js/camera_napi/include/camera_napi_event_emitter.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 4 |
| 37 | `interfaces/kits/js/camera_napi/include/session/camera_napi_adaptor.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 5 |
| 38 | `mediastream/test/unittest/filter/src/audio_encoder_filter_unit_test.cpp` | tree-sitter-cpp node `missing ;` at 2 site(s) | 2 |
| 39 | `moviefile/include/movie_file/plugin/movie_file_video_filter_plugin.h` | tree-sitter-cpp node `missing field_identifier` at 1 site(s) | 1 |
| 40 | `services/camera_service/binder/base/include/icamera_broker.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 41 | `services/camera_service/binder/base/include/icamera_multi_stream_output.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 42 | `services/camera_service/binder/base/include/istream_capture_photo_callback.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 43 | `services/camera_service/binder/base/include/istream_capture_thumbnail_callback.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 44 | `services/camera_service/binder/base/include/window_manager_service_utils/icamera_mock_session_manager_interface.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 45 | `services/camera_service/binder/base/include/window_manager_service_utils/icamera_scene_session_manager.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 46 | `services/camera_service/binder/base/include/window_manager_service_utils/icamera_window_manager_callback.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 47 | `services/camera_service/binder/base/include/window_manager_service_utils/icamera_window_session_manager_service.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 48 | `services/camera_service/include/param_update/camera_rotate_param_manager.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 2 |
| 49 | `services/camera_service/src/camera_beauty_notification.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 50 | `services/camera_service/src/hcamera_device.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 51 | `services/camera_service/src/hcamera_service.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 52 | `services/deferred_processing_service/include/base/blocking_queue.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 57 |
| 53 | `services/deferred_processing_service/include/base/dps.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 54 | `services/deferred_processing_service/include/deferred_processing_service.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 55 | `services/deferred_processing_service/include/dfx/dps_video_report.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 56 | `services/deferred_processing_service/include/event_monitor/events_monitor.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 57 | `services/deferred_processing_service/include/schedule/video_processor/strategy/ivideo_strategy.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 4 |
| 58 | `services/deferred_processing_service/include/schedule/video_processor/video_job_repository/ivideo_job_repository_listener.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 3 |
| 59 | `services/deferred_processing_service/include/utils/dp_power_manager.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 60 | `services/deferred_processing_service/include/utils/dp_safe_map.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 37 |
| 61 | `services/deferred_processing_service/include/utils/dp_timer.h` | tree-sitter-cpp node `missing ;` at 1 site(s) | 1 |
| 62 | `test/fuzztest/audiodeferredprocess_fuzzer/audio_deferred_process_fuzzer.cpp` | generic tree-sitter ERROR node(s) in preprocessed C++ | 6 |
| 63 | `test/fuzztest/cameraserviceproxy_fuzzer/camera_service_proxy_fuzzer.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |
| 64 | `test/fuzztest/hcameraservice_fuzzer/hcamera_service_fuzzer.h` | generic tree-sitter ERROR node(s) in preprocessed C++ | 1 |

### Per-file details

#### `common/utils/media_capability_interface.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 5 | 11 | `ERROR` | `OHOS::` |
| 6 | 26 | `ERROR` | `: public Parcelable` |
| 8 | 8 | `ERROR` | `~` |
| 9 | 9 | `ERROR` | `bool` |
| 10 | 49 | `ERROR` | `&` |

#### `dynamic_libs/media_manager/include/media_manager/mpeg_manager_factory.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 38 | `missing ;` | `` |

#### `dynamic_libs/media_manager/include/media_manager/track_factory.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 32 | `missing ;` | `` |

#### `dynamic_libs/moving_photo/src/moving_photo_adapter.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 347 | 1 | `ERROR` | `__attribute__((visibility("default"))) extern` |
| 347 | 50 | `missing ;` | `` |
| 352 | 1 | `ERROR` | `__attribute__((visibility("default"))) extern` |
| 352 | 50 | `missing ;` | `` |
| 357 | 1 | `ERROR` | `__attribute__((visibility("default"))) extern` |
| 357 | 50 | `missing ;` | `` |
| 362 | 1 | `ERROR` | `__attribute__((visibility("default"))) extern` |
| 362 | 50 | `missing ;` | `` |
| 367 | 1 | `ERROR` | `__attribute__((visibility("default"))) extern` |
| 367 | 50 | `missing ;` | `` |

#### `frameworks/cj/camera/include/camera_ffi.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 10 | 12 | `ERROR` | `int64_t` |
| 11 | 12 | `ERROR` | `CArrI32` |
| 12 | 30 | `missing ::` | `` |
| 13 | 36 | `missing ::` | `` |
| 16 | 12 | `ERROR` | `bool` |
| 17 | 12 | `ERROR` | `int64_t` |
| 19 | 12 | `ERROR` | `int64_t` |
| 21 | 12 | `ERROR` | `int64_t` |
| 22 | 12 | `ERROR` | `RetInt64` |
| 23 | 12 | `ERROR` | `int64_t` |
| 24 | 12 | `ERROR` | `RetInt64` |
| 25 | 12 | `ERROR` | `int64_t` |
| 26 | 12 | `ERROR` | `int64_t` |
| 27 | 12 | `ERROR` | `int64_t` |
| 28 | 12 | `ERROR` | `int64_t` |
| 29 | 12 | `ERROR` | `int64_t` |
| 30 | 12 | `ERROR` | `int64_t` |
| 31 | 12 | `ERROR` | `bool` |
| 32 | 12 | `ERROR` | `bool` |
| 33 | 12 | `ERROR` | `int32_t` |
| … | … | … | *(144 more)* |

#### `frameworks/cj/camera/include/camera_input_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 15 | 48 | `missing ;` | `` |

#### `frameworks/cj/camera/include/camera_manager_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 30 | 50 | `missing ;` | `` |

#### `frameworks/cj/camera/include/camera_output_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 8 | 47 | `missing ;` | `` |

#### `frameworks/cj/camera/include/camera_session_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 32 | 44 | `missing ;` | `` |

#### `frameworks/cj/camera/include/metadata_output_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 25 | 51 | `missing ;` | `` |

#### `frameworks/cj/camera/include/photo_output_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 45 | 48 | `missing ;` | `` |

#### `frameworks/cj/camera/include/preview_output_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 27 | 50 | `missing ;` | `` |

#### `frameworks/cj/camera/include/video_output_impl.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 27 | 48 | `missing ;` | `` |

#### `frameworks/cj/camera/src/camera_ffi.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 631 | 12 | `ERROR` | `int32_t` |
| 1333 | 12 | `ERROR` | `bool` |
| 1348 | 12 | `ERROR` | `void` |
| 1360 | 12 | `ERROR` | `void` |
| 1377 | 12 | `ERROR` | `void` |
| 1389 | 12 | `ERROR` | `void` |
| 1401 | 12 | `ERROR` | `void` |
| 1413 | 12 | `ERROR` | `void` |
| 1425 | 12 | `ERROR` | `void` |
| 1437 | 12 | `ERROR` | `void` |
| 1449 | 12 | `ERROR` | `void` |
| 1461 | 12 | `ERROR` | `void` |
| 1473 | 12 | `ERROR` | `void` |
| 1485 | 12 | `ERROR` | `bool` |
| 1498 | 23 | `missing ::` | `` |
| 1511 | 12 | `ERROR` | `int32_t` |
| 1524 | 12 | `ERROR` | `void` |
| 1535 | 12 | `ERROR` | `Point` |
| 1548 | 12 | `ERROR` | `void` |
| 1559 | 12 | `ERROR` | `void` |
| … | … | … | *(22 more)* |

#### `frameworks/cj/camera_picker/include/camera_picker_ffi.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 10 | 12 | `ERROR` | `void` |

#### `frameworks/native/camera/base/src/ability/camera_ability_builder.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 193 | 2 | `ERROR` | `this->*((` |
| 193 | 35 | `ERROR` | `))(dataArray` |
| 193 | 57 | `ERROR` | `)` |

#### `frameworks/native/camera/test/ndktest/camera_ndk_demo/entry/src/main/cpp/main.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 468 | 8 | `ERROR` | `napi_value` |
| 500 | 20 | `ERROR` | `demoModule` |

#### `frameworks/native/camera/test/unittest/camera_deferred_unittest/camera_deferred_schedule_test/include/camera_deferred_video_unittest.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 7 | 7 | `ERROR` | `TestDeferredVideoProcSessionCallback: public` |
| 12 | 37 | `ERROR` | `:: string&` |
| 12 | 68 | `ERROR` | `< IPCFileDescriptor> &` |
| 13 | 26 | `ERROR` | `:: string&` |
| 17 | 28 | `ERROR` | `: public testing:: Test` |
| 36 | 1 | `ERROR` | `sptr< DeferredProcessing::` |
| 36 | 69 | `missing ;` | `` |
| 36 | 78 | `ERROR` | `}` |
| 37 | 4 | `ERROR` | `:: shared_ptr< DeferredProcessing:` |
| 37 | 71 | `missing ;` | `` |
| 37 | 80 | `ERROR` | `}` |
| 38 | 4 | `ERROR` | `:: shared_ptr< DeferredProcessing:` |
| 38 | 75 | `missing ;` | `` |
| 38 | 84 | `ERROR` | `}` |
| 39 | 4 | `ERROR` | `:: shared_ptr< DeferredProcessing:` |
| 39 | 74 | `missing ;` | `` |
| 39 | 83 | `ERROR` | `}` |

#### `frameworks/native/camera/test/unittest/camera_deferred_unittest/camera_deferred_session_test/src/deferred_photo_session_unittest.cpp`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 66 | 84 | `missing ;` | `` |

#### `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/include/hcamera_device_unittest.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 10 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |

#### `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/include/hcamera_service_unittest.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 30 | `ERROR` | `u"` |
| 20 | 53 | `ERROR` | `"` |
| 35 | 30 | `ERROR` | `u"` |
| 35 | 52 | `ERROR` | `"` |
| 49 | 30 | `ERROR` | `u"` |
| 49 | 60 | `ERROR` | `"` |
| 64 | 30 | `ERROR` | `u"` |
| 64 | 62 | `ERROR` | `"` |
| 69 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |
| 101 | 30 | `ERROR` | `u"` |
| 101 | 58 | `ERROR` | `"` |

#### `frameworks/native/camera/test/unittest/camera_service/hdi_camera_test/src/hcamera_device_manager_unittest.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 12 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |

#### `frameworks/native/camera/test/unittest/camera_service/hdi_stream_test/include/hstream_operator_unittest.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 29 | 79 | `missing type_identifier` | `` |

#### `frameworks/native/camera/test/unittest/camera_service/hdi_stream_test/src/hstream_capture_unittest.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 17 | 1 | `ERROR` | `class IStreamOperatorFork: public IStreamOperator{ public: DECLARE_HDI_DESCRIPTOR(u` |
| 19 | 63 | `ERROR` | `)` |
| 1210 | 1 | `ERROR` | `}` |

#### `frameworks/native/camera/test/unittest/framework_native/output/src/photo_output_unittest.cpp`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 62 | 74 | `missing ;` | `` |

#### `frameworks/native/camera/test/unittest/framework_native/session/src/capture_session_unittest.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 3666 | 25 | `ERROR` | `.operator()` |

#### `frameworks/native/ndk/impl/camera_manager_impl.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 342 | 52 | `ERROR` | `*[previewProfiles` |
| 342 | 77 | `ERROR` | `]` |
| 366 | 50 | `ERROR` | `*[photoProfiles` |
| 366 | 73 | `ERROR` | `]` |
| 388 | 55 | `ERROR` | `*[videoProfiles` |
| 388 | 78 | `ERROR` | `]` |
| 412 | 76 | `ERROR` | `*[metadataTypeList` |
| 412 | 102 | `ERROR` | `]` |

#### `frameworks/native/ndk/impl/metadata_output_impl.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 209 | 75 | `ERROR` | `*[size]` |

#### `frameworks/taihe/include/camera_event_emitter_taihe.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 14 | `ERROR` | `T::` |
| 12 | 14 | `ERROR` | `T::` |
| 25 | 11 | `ERROR` | `->` |
| 40 | 11 | `ERROR` | `->` |

#### `frameworks/taihe/src/camera_constructor_taihe.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 2 | 12 | `ERROR` | `ani_status` |

#### `frameworks/taihe/src/camera_picker_constructor_taihe.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 3 | 12 | `ERROR` | `ani_status` |

#### `interfaces/inner_api/native/camera/include/ability/camera_ability_builder.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 12 | 14 | `ERROR` | `CameraAbilityBuilder::` |

#### `interfaces/inner_api/native/camera/include/input/camera_manager.h`

**Summary:** tree-sitter-cpp node `missing type_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 617 | 63 | `missing type_identifier` | `` |

#### `interfaces/inner_api/native/camera/include/input/i_standard_camera_listener.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 30 | `ERROR` | `u"` |
| 11 | 55 | `ERROR` | `"` |

#### `interfaces/inner_api/native/camera/include/output/photo_output.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 78 | 13 | `missing }` | `` |
| 78 | 27 | `missing field_identifier` | `` |
| 81 | 1 | `ERROR` | `}` |

#### `interfaces/kits/js/camera_napi/include/camera_napi_event_emitter.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 14 | `ERROR` | `T::` |
| 13 | 14 | `ERROR` | `T::` |
| 29 | 11 | `ERROR` | `->` |
| 46 | 11 | `ERROR` | `->` |

#### `interfaces/kits/js/camera_napi/include/session/camera_napi_adaptor.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 77 | 30 | `ERROR` | `*` |
| 100 | 30 | `ERROR` | `*` |
| 131 | 34 | `ERROR` | `*` |
| 152 | 27 | `ERROR` | `*` |
| 175 | 27 | `ERROR` | `*` |

#### `mediastream/test/unittest/filter/src/audio_encoder_filter_unit_test.cpp`

**Summary:** tree-sitter-cpp node `missing ;` at 2 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 185 | 113 | `missing ;` | `` |
| 451 | 117 | `missing ;` | `` |

#### `moviefile/include/movie_file/plugin/movie_file_video_filter_plugin.h`

**Summary:** tree-sitter-cpp node `missing field_identifier` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 15 | 13 | `missing field_identifier` | `` |

#### `services/camera_service/binder/base/include/icamera_broker.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |

#### `services/camera_service/binder/base/include/icamera_multi_stream_output.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 7 | 31 | `ERROR` | `: public IRemoteBroker` |

#### `services/camera_service/binder/base/include/istream_capture_photo_callback.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 14 | 30 | `ERROR` | `u"` |
| 14 | 59 | `ERROR` | `"` |

#### `services/camera_service/binder/base/include/istream_capture_thumbnail_callback.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 15 | 30 | `ERROR` | `u"` |
| 15 | 63 | `ERROR` | `"` |

#### `services/camera_service/binder/base/include/window_manager_service_utils/icamera_mock_session_manager_interface.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 31 | `ERROR` | `"OHOS.IMockSessionManager"` |

#### `services/camera_service/binder/base/include/window_manager_service_utils/icamera_scene_session_manager.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 13 | 31 | `ERROR` | `"OHOS.IWindowManager"` |
| 28 | 31 | `ERROR` | `"OHOS.ISceneSessionManager"` |

#### `services/camera_service/binder/base/include/window_manager_service_utils/icamera_window_manager_callback.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 12 | 31 | `ERROR` | `"OHOS.IWindowManagerAgent"` |

#### `services/camera_service/binder/base/include/window_manager_service_utils/icamera_window_session_manager_service.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 31 | `ERROR` | `"OHOS.ISessionManagerService"` |

#### `services/camera_service/include/param_update/camera_rotate_param_manager.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 59 | 14 | `ERROR` | `CameraRoateParamManager::` |
| 59 | 79 | `ERROR` | `const` |

#### `services/camera_service/src/camera_beauty_notification.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 44 | 14 | `ERROR` | `.operator++` |

#### `services/camera_service/src/hcamera_device.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 682 | 19 | `ERROR` | `.operator++` |

#### `services/camera_service/src/hcamera_service.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 1 | `ERROR` | `REGISTER_SYSTEM_ABILITY_BY_ID(HCameraService , CAMERA_SERVICE_ID , true) constexpr` |

#### `services/deferred_processing_service/include/base/blocking_queue.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 0 | 0 | `?` | `line 11 col 66 (missing )) ` |
| 8 | 20 | `ERROR` | `T` |
| 11 | 33 | `ERROR` | `:: string&` |
| 11 | 68 | `ERROR` | `0) :` |
| 12 | 35 | `ERROR` | `> 0? capacity: std:: numeric_limits< size_t>::` |
| 12 | 91 | `ERROR` | `isActive_(true) { } ~` |
| 18 | 4 | `ERROR` | `:: unique_lock< std:` |
| 23 | 4 | `ERROR` | `:: unique_lock< std:` |
| 32 | 4 | `ERROR` | `:: unique_lock< std:` |
| 35 | 18 | `ERROR` | `&` |
| 37 | 4 | `ERROR` | `:: unique_lock< std:` |
| 38 | 9 | `ERROR` | `=` |
| 45 | 18 | `ERROR` | `&` |
| 47 | 4 | `ERROR` | `:: unique_lock< std:` |
| 48 | 9 | `ERROR` | `=` |
| 55 | 12 | `ERROR` | `&&` |
| 57 | 4 | `ERROR` | `:: unique_lock< std:` |
| 58 | 9 | `ERROR` | `=` |
| 60 | 12 | `ERROR` | `std::` |
| 67 | 4 | `ERROR` | `:: unique_lock< std:` |
| … | … | … | *(37 more)* |

#### `services/deferred_processing_service/include/base/dps.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 22 | 24 | `ERROR` | `*` |

#### `services/deferred_processing_service/include/deferred_processing_service.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 45 | `missing ;` | `` |

#### `services/deferred_processing_service/include/dfx/dps_video_report.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 31 | 34 | `missing ;` | `` |

#### `services/deferred_processing_service/include/event_monitor/events_monitor.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 20 | 33 | `missing ;` | `` |

#### `services/deferred_processing_service/include/schedule/video_processor/strategy/ivideo_strategy.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 8 | `ERROR` | `~` |
| 13 | 29 | `missing ;` | `` |
| 14 | 28 | `missing ;` | `` |
| 15 | 22 | `missing ;` | `` |

#### `services/deferred_processing_service/include/schedule/video_processor/video_job_repository/ivideo_job_repository_listener.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 11 | 8 | `ERROR` | `~` |
| 13 | 9 | `ERROR` | `void` |
| 13 | 57 | `ERROR` | `&` |

#### `services/deferred_processing_service/include/utils/dp_power_manager.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 36 | `missing ;` | `` |

#### `services/deferred_processing_service/include/utils/dp_safe_map.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 8 | 20 | `ERROR` | `K` |
| 8 | 33 | `ERROR` | `V` |
| 11 | 13 | `missing ;` | `` |
| 13 | 15 | `missing ;` | `` |
| 15 | 28 | `ERROR` | `& rhs` |
| 16 | 1 | `ERROR` | `{` |
| 20 | 23 | `ERROR` | `const DpsSafeMap` |
| 20 | 45 | `missing ;` | `` |
| 25 | 9 | `missing identifier` | `` |
| 26 | 4 | `ERROR` | `:: lock_guard< std:` |
| 27 | 1 | `ERROR` | `map_= std::` |
| 32 | 19 | `ERROR` | `...Args` |
| 33 | 5 | `missing ;` | `` |
| 33 | 13 | `ERROR` | `(const K` |
| 33 | 35 | `ERROR` | `...` |
| 35 | 4 | `ERROR` | `:: lock_guard< std:` |
| 36 | 21 | `ERROR` | `std::` |
| 36 | 47 | `ERROR` | `...` |
| 39 | 26 | `ERROR` | `&` |
| 39 | 41 | `ERROR` | `&` |
| … | … | … | *(17 more)* |

#### `services/deferred_processing_service/include/utils/dp_timer.h`

**Summary:** tree-sitter-cpp node `missing ;` at 1 site(s)

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 9 | 28 | `missing ;` | `` |

#### `test/fuzztest/audiodeferredprocess_fuzzer/audio_deferred_process_fuzzer.cpp`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 10 | 31 | `missing ;` | `` |
| 10 | 32 | `ERROR` | `__attribute__((used))=` |
| 12 | 25 | `missing ;` | `` |
| 12 | 26 | `ERROR` | `__attribute__((used))=` |
| 13 | 20 | `missing ;` | `` |
| 13 | 21 | `ERROR` | `__attribute__((used))` |

#### `test/fuzztest/cameraserviceproxy_fuzzer/camera_service_proxy_fuzzer.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 94 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |

#### `test/fuzztest/hcameraservice_fuzzer/hcamera_service_fuzzer.h`

**Summary:** generic tree-sitter ERROR node(s) in preprocessed C++

| Line | Col | Node kind | Snippet |
|-----:|----:|-----------|---------|
| 122 | 31 | `ERROR` | `"OHOS.Anco.Service.Camera"` |

---
