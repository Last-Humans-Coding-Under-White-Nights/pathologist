# Conditional-compilation coverage — eval corpora

What the single default configuration excludes (#57): every `#if` / `#ifdef` / `#ifndef` chain, which arm each preprocess run took, the source lines in the arms not taken, and what the checkout knows about the names the conditions read. Reporting only — preprocessing behaviour is unchanged. Regenerate with:

```bash
set -euo pipefail   # stop at the first failure, do not run on with stale inputs

export TRACE_CORPUS_BASE=/private/tmp/corpora
python3 scripts/fetch_corpora.py   # corpora at the revisions pinned in scripts/eval_expected.json
cargo build --release -p trace-cli --examples

# Each TSV is written to a .part file and renamed only if the command
# succeeded; the generator treats a MISSING file as an error.
rm -f /tmp/conditional_coverage_{hdf,hiview,camera}.tsv{,.part}
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/drivers_hdf_core" > /tmp/conditional_coverage_hdf.tsv.part
mv /tmp/conditional_coverage_hdf.tsv.part /tmp/conditional_coverage_hdf.tsv
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/hiviewdfx_hiview" > /tmp/conditional_coverage_hiview.tsv.part
mv /tmp/conditional_coverage_hiview.tsv.part /tmp/conditional_coverage_hiview.tsv
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/multimedia_camera_framework" > /tmp/conditional_coverage_camera.tsv.part
mv /tmp/conditional_coverage_camera.tsv.part /tmp/conditional_coverage_camera.tsv

python3 scripts/gen_conditional_coverage_report.py
```

## How to read this

- **The record is the chain, not the macro.** A chain is one `#if`/`#ifdef`/`#ifndef` with its `#elif`/`#else` arms. The lines an arm excludes belong to the whole expression that controls the chain; for `#if A && B` crediting them to `A` and to `B` separately double-counts and overstates what defining either one would recover. The per-name view below therefore splits lines into *sole* (the name is the chain's only dependency) and *shared* (listed under every name of the chain).
- **Environment.** Every translation unit is preprocessed from the command-line defines alone with its includes expanded inline — the environment `trace analyze` gives each unit — and headers no unit reaches are preprocessed standalone, as the indexer does with orphans. No expansion cache: a cache hit replays a header's text without re-evaluating its conditionals. A header reached from several units is evaluated once per unit, so an arm can be taken in some runs and not in others (*sometimes excluded*); *always excluded* arms were never taken by any run. File totals include headers resolved outside the root through `--include`. Missing or empty source trees and hard input failures stop the measurement without publishing TSV. Command-line metadata retains `-D` values. A final completion record counts all preceding TSV rows; missing or mismatched completion records are rejected, including captures cut off at a complete line. Older TSV files must be regenerated.
- **Which arm.** An undefined name does not always select `#else`: `#if !X` with `X` unknown takes the first arm. Each arm's outcome is recorded per run rather than assumed.
- **Names read** are what the evaluation consulted, macro expansion included (`#if HAS_X` with `#define HAS_X defined(X)` reads both). *Unbound reads* count the evaluations that found no macro bound to the name — the cases that resolved against the default of `0`. An arm that was never evaluated contributes only the identifiers it spells.
- **Classes.** *include-guard*: tested by a chain that wraps a whole file (`#ifndef X` first, `#define X` next, no `#else`, nothing after its `#endif`) and by no other chain — a default-value idiom alone in a file has the guard shape, and an `#if X > 1` elsewhere that depends on the name says it is configuration. *toolchain*: a macro gcc/clang predefine (a fixed list — language, compiler, target OS, architecture, type sizes, `__has_*`). *configuration*: a `-D`, an in-tree `#define` in any region (comments and string literals ignored), or a name an in-tree build file spells (GN, CMake, Make, Kconfig — spelled, not parsed; #58 is the ranked version). *unknown*: nothing in the checkout accounts for it. Unknown is a real category, not a failure to classify: #59 needs to know which names it cannot reason about.
- **Lines are source lines** strictly between the arm's directive and the next directive of its chain, not reachable code: a nested chain's directive lines count, blank and comment lines and continuation lines of multiline conditions count, and an always-excluded outer arm hides its inner chains (they are *never evaluated* and add nothing). Sometimes-excluded lines of nested chains can overlap. Excluded lines are not an acceptance metric on their own — what matters for #59 is whether the excluded arms hold new, source-verified driver and callback targets.

## Overview

| Corpus | Files | Source lines | Chains | Always excluded | Sometimes excluded | Unknown names | … controlling excluded arms |
|--------|------:|-------------:|-------:|----------------:|-------------------:|--------------:|----------------------------:|
| `drivers_hdf_core` | 1,483 | 300,071 | 2,286 | 6,980 (2.3%) | 387 (0.1%) | 98 | 86 |
| `hiviewdfx_hiview` | 1,428 | 173,562 | 1,199 | 7,387 (4.3%) | 1 (0.0%) | 16 | 13 |
| `multimedia_camera_framework` | 1,593 | 403,098 | 1,257 | 4,991 (1.2%) | 35 (0.0%) | 4 | 4 |

## Always-excluded lines by name class, sole dependency

| Class | HDF | Hiview | Camera |
|-------|----:|-------:|-------:|
| configuration | 3,812 | 6,744 | 4,976 |
| unknown | 1,417 | 379 | 13 |
| toolchain | 837 | 62 | 0 |
| include-guard | 0 | 0 | 0 |

## drivers_hdf_core

Generated from `conditional_coverage /private/tmp/corpora/drivers_hdf_core` with `-D` defines: (none).

| | |
|---|---:|
| Files preprocessed (translation units + headers) | 1,483 (802 + 681) |
| Headers evaluated by more than one unit | 439 |
| Source lines | 300,071 |
| Conditional chains | 2,286 |
| … include guards | 681 |
| … evaluated by at least one run | 1,999 |
| … never evaluated (inside excluded code in every run) | 287 |
| … left unterminated | 0 |
| **Lines always excluded** | **6,980 (2.3%)** |
| Lines excluded in some runs, included in others | 387 (0.1%) |
| Files with an always-excluded arm | 341 |

### Which arm is taken

| Chains (guards excluded) | Count |
|--------------------------|------:|
| Evaluated by at least one run | 1,318 |
| First arm never taken | 910 |
| … of which an `#else` arm was always taken | 186 |
| … of which no arm was ever taken (no `#else`, or every arm false) | 724 |
| First arm always taken | 205 |
| First arm taken in some runs, not in others | 203 |

| Always-excluded arms by directive | Arms | Lines |
|-----------------------------------|-----:|------:|
| `#if` | 195 | 1,731 |
| `#ifdef` | 715 | 5,154 |
| `#ifndef` | 0 | 0 |
| `#elif` | 12 | 54 |
| `#else` | 22 | 41 |

### Excluded lines by name class

| Class | Names | Chains reading them | Lines excluded, sole dependency | Lines excluded, shared | Unbound reads |
|-------|------:|--------------------:|-------------------------------:|-----------------------:|--------------:|
| configuration | 109 | 657 | 3,812 | 1,059 | 4,757 |
| unknown | 98 | 270 | 1,417 | 535 | 976 |
| toolchain | 9 | 924 | 837 | 544 | 21,256 |
| include-guard | 627 | 681 | 0 | 0 | 14,126 |

### Top expressions by lines always excluded

| Expression (chain as written) | Regions | Files | Lines always excluded | Lines sometimes excluded | Names (class) |
|-------------------------------|--------:|------:|----------------------:|-------------------------:|---------------|
| `#ifdef SAMPLE_DRIVER` | 24 | 5 | 1,769 | 0 | `SAMPLE_DRIVER` (configuration) |
| `#ifdef __cplusplus` | 607 | 304 | 832 | 305 | `__cplusplus` (toolchain) |
| `#ifdef LOSCFG_DRIVERS_HDF_CONFIG_MACRO / #else` | 52 | 17 | 563 | 0 | `LOSCFG_DRIVERS_HDF_CONFIG_MACRO` (configuration) |
| `#ifdef __USER__` | 49 | 11 | 531 | 0 | `__USER__` (configuration) |
| `#ifdef LOSCFG_FS_VFS / #else` | 3 | 2 | 502 | 0 | `LOSCFG_FS_VFS` (unknown) |
| `#if defined(CONFIG_DRIVERS_HDF_IMX8MM_ETHERNET)` | 9 | 5 | 331 | 0 | `CONFIG_DRIVERS_HDF_IMX8MM_ETHERNET` (unknown) |
| `#if USB_PNP_NOTIFY_TEST_MODE == true` | 15 | 4 | 254 | 0 | `USB_PNP_NOTIFY_TEST_MODE` (configuration), `false` (toolchain), `true` (toolchain) |
| `#if defined(CONFIG_ARCH_ROCKCHIP)` | 11 | 2 | 188 | 0 | `CONFIG_ARCH_ROCKCHIP` (configuration) |
| `#if GTP_ESD_PROTECT` | 2 | 1 | 173 | 0 | `GTP_ESD_PROTECT` (configuration) |
| `#if defined(LOSCFG_DRIVERS_HDF_PLATFORM) \|\| defined(CONFIG_DRIVERS_HDF_PLATFORM)` | 2 | 1 | 132 | 0 | `LOSCFG_DRIVERS_HDF_PLATFORM` (configuration), `CONFIG_DRIVERS_HDF_PLATFORM` (configuration) |
| `#ifdef __LITEOS__` | 21 | 17 | 129 | 0 | `__LITEOS__` (configuration) |
| `#if defined(HDF_LITEOS_TEST)` | 2 | 1 | 125 | 0 | `HDF_LITEOS_TEST` (configuration) |
| `#if defined(LOSCFG_USER_I2C_SUPPORT) \|\| defined(CONFIG_USER_I2C_SUPPORT)` | 2 | 1 | 124 | 0 | `LOSCFG_USER_I2C_SUPPORT` (unknown), `CONFIG_USER_I2C_SUPPORT` (unknown) |
| `#ifdef __USER__ / #else` | 20 | 5 | 117 | 0 | `__USER__` (configuration) |
| `#ifdef LOSCFG_KERNEL_TRACE` | 6 | 1 | 66 | 0 | `LOSCFG_KERNEL_TRACE` (unknown) |
| `#ifdef __LITEOS__ / #else` | 20 | 8 | 54 | 0 | `__LITEOS__` (configuration) |
| `#ifdef WITH_SELINUX` | 6 | 3 | 51 | 0 | `WITH_SELINUX` (unknown) |
| `#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 6, 0)` | 32 | 3 | 44 | 0 | `LINUX_VERSION_CODE` (configuration), `KERNEL_VERSION` (configuration) |
| `#if defined(LOSCFG_DRIVERS_HDF_PLATFORM_SPI) \|\| defined(CONFIG_DRIVERS_HDF_PLATFORM_SPI)` | 6 | 3 | 38 | 0 | `LOSCFG_DRIVERS_HDF_PLATFORM_SPI` (configuration), `CONFIG_DRIVERS_HDF_PLATFORM_SPI` (configuration) |
| `#ifdef HDFHICOLLIE_ENABLE / #else` | 3 | 2 | 37 | 0 | `HDFHICOLLIE_ENABLE` (configuration) |
| `#ifdef __KERNEL__ / #else` | 4 | 3 | 37 | 0 | `__KERNEL__` (unknown) |
| `#ifdef LOSCFG_BASE_CORE_HILOG / #else` | 3 | 3 | 36 | 0 | `LOSCFG_BASE_CORE_HILOG` (unknown) |
| `#if defined(CONFIG_ARCH_ROCKCHIP) / #else` | 2 | 1 | 34 | 0 | `CONFIG_ARCH_ROCKCHIP` (configuration) |
| `#ifdef MIPI_TX_DEBUG` | 6 | 1 | 34 | 0 | `MIPI_TX_DEBUG` (unknown) |
| `#ifdef LOSCFG_DRIVERS_HDF_USB_PNP_NOTIFY / #else` | 2 | 2 | 28 | 0 | `LOSCFG_DRIVERS_HDF_USB_PNP_NOTIFY` (configuration) |
| `#ifdef LOSCFG_SOC_SERIES_BES2600` | 3 | 1 | 27 | 0 | `LOSCFG_SOC_SERIES_BES2600` (unknown) |
| `#if defined(CONFIG_ARCH_SPRD) / #elif defined(CONFIG_ARCH_ROCKCHIP) / #elif defined(LOSCFG_PLATFORM_STM32MP157) / #else` | 1 | 1 | 26 | 0 | `CONFIG_ARCH_SPRD` (configuration), `CONFIG_ARCH_ROCKCHIP` (configuration), `LOSCFG_PLATFORM_STM32MP157` (unknown) |
| `#ifdef LOSCFG_NET_LWIP_SACK_2_0` | 3 | 2 | 26 | 0 | `LOSCFG_NET_LWIP_SACK_2_0` (unknown) |
| `#ifdef __KERNEL__` | 2 | 2 | 26 | 0 | `__KERNEL__` (unknown) |
| `#if defined(KERNEL_SERVER_SUPPORT) \|\| defined(USERSPACE_CLIENT_SUPPORT)` | 3 | 2 | 22 | 0 | `KERNEL_SERVER_SUPPORT` (unknown), `USERSPACE_CLIENT_SUPPORT` (unknown) |
| `#ifdef CONFIG_DRIVERS_HDF_NETDEV_EXT` | 1 | 1 | 21 | 0 | `CONFIG_DRIVERS_HDF_NETDEV_EXT` (unknown) |
| `#if defined (LOSCFG_SOC_SERIES_BES2700) / #elif defined (LOSCFG_SOC_SERIES_BES2600)` | 3 | 2 | 19 | 0 | `LOSCFG_SOC_SERIES_BES2700` (unknown), `LOSCFG_SOC_SERIES_BES2600` (unknown) |
| `#ifdef MIPI_CSI_TEST_SELF / #else` | 1 | 1 | 19 | 0 | `MIPI_CSI_TEST_SELF` (unknown) |
| `#if defined(__LITEOS__)` | 6 | 1 | 17 | 0 | `__LITEOS__` (configuration) |
| `#ifdef WATCHDOG_TEST_BARK_RESET / #else` | 1 | 1 | 16 | 0 | `WATCHDOG_TEST_BARK_RESET` (unknown) |
| `#ifdef LOSCFG_KERNEL_SMP / #else` | 2 | 1 | 14 | 0 | `LOSCFG_KERNEL_SMP` (unknown) |
| `#ifdef LOSCFG_SOC_SERIES_BES2600 / #elif defined (LOSCFG_SOC_SERIES_BES2700)` | 1 | 1 | 14 | 0 | `LOSCFG_SOC_SERIES_BES2600` (unknown), `LOSCFG_SOC_SERIES_BES2700` (unknown) |
| `#ifdef USERSPACE_CLIENT_SUPPORT / #else` | 5 | 4 | 13 | 0 | `USERSPACE_CLIENT_SUPPORT` (unknown) |
| `#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0) / #else` | 2 | 2 | 12 | 0 | `LINUX_VERSION_CODE` (configuration), `KERNEL_VERSION` (configuration) |
| `#ifdef LOSCFG_SOC_SERIES_BES2700 / #elif defined (LOSCFG_SOC_SERIES_BES2600)` | 1 | 1 | 12 | 0 | `LOSCFG_SOC_SERIES_BES2700` (unknown), `LOSCFG_SOC_SERIES_BES2600` (unknown) |

### Names, derived view

Lines are apportioned as described above: *sole* when the name is the only one the chain reads, *shared* otherwise (a shared line is listed under every name of its chain).

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `SAMPLE_DRIVER` | configuration | 24 | 1,769 | 0 | 0 | 0 / 24 | — | `adapter/uhdf2/hdi/test/BUILD.gn` |
| `__cplusplus` | toolchain | 880 | 832 | 0 | 307 | 1,518 / 17,028 | — | — |
| `__USER__` | configuration | 102 | 650 | 9 | 0 | 0 / 1,006 | — | `adapter/uhdf/manager/BUILD.gn` |
| `LOSCFG_DRIVERS_HDF_CONFIG_MACRO` | configuration | 54 | 563 | 0 | 0 | 0 / 456 | — | `adapter/khdf/liteos_m/BUILD.gn` |
| `LOSCFG_FS_VFS` | unknown | 3 | 502 | 0 | 0 | 0 / 3 | — | — |
| `CONFIG_DRIVERS_HDF_IMX8MM_ETHERNET` | unknown | 9 | 331 | 0 | 0 | 0 / 75 | — | — |
| `USB_PNP_NOTIFY_TEST_MODE` | configuration | 17 | 0 | 256 | 0 | 22 / 0 | `framework/model/usb/include/hdf_usb_pnp_manage.h:14` | — |
| `false` | toolchain | 17 | 0 | 256 | 0 | 0 / 22 | — | `adapter/khdf/liteos_m/hdf.gni` |
| `CONFIG_ARCH_ROCKCHIP` | configuration | 18 | 224 | 30 | 0 | 0 / 18 | — | `adapter/khdf/linux/model/display/Makefile` |
| `true` | toolchain | 15 | 0 | 254 | 0 | 0 / 20 | — | `adapter/BUILD.gn` |
| `__LITEOS__` | configuration | 75 | 203 | 16 | 0 | 0 / 1,027 | — | `adapter/uhdf/test/unittest/manager/BUILD.gn` |
| `GTP_ESD_PROTECT` | configuration | 5 | 173 | 0 | 0 | 0 / 2 | `framework/model/input/driver/hdf_touch.c:35` | — |
| `CONFIG_DRIVERS_HDF_PLATFORM` | configuration | 2 | 0 | 132 | 0 | 0 / 2 | — | `adapter/khdf/linux/Makefile` |
| `LOSCFG_DRIVERS_HDF_PLATFORM` | configuration | 2 | 0 | 132 | 0 | 0 / 2 | — | `adapter/khdf/liteos/hdf_lite.mk` |
| `HDF_LITEOS_TEST` | configuration | 3 | 125 | 0 | 0 | 0 / 3 | — | `adapter/uhdf/test/unittest/platform/BUILD.gn` |
| `CONFIG_USER_I2C_SUPPORT` | unknown | 2 | 0 | 124 | 0 | 0 / 2 | — | — |
| `LOSCFG_USER_I2C_SUPPORT` | unknown | 2 | 0 | 124 | 0 | 0 / 2 | — | — |
| `__KERNEL__` | unknown | 31 | 84 | 7 | 0 | 0 / 149 | — | — |
| `KERNEL_VERSION` | configuration | 57 | 0 | 89 | 0 | 0 / 57 | — | `adapter/khdf/linux/model/display/Makefile` |
| `LINUX_VERSION_CODE` | configuration | 57 | 0 | 89 | 0 | 0 / 57 | — | `adapter/khdf/linux/model/display/Makefile` |
| `LOSCFG_SOC_SERIES_BES2600` | unknown | 9 | 31 | 45 | 0 | 0 / 9 | — | — |
| `LOSCFG_KERNEL_TRACE` | unknown | 7 | 66 | 0 | 0 | 0 / 6 | — | — |
| `WITH_SELINUX` | unknown | 7 | 56 | 0 | 0 | 0 / 7 | — | — |
| `LOSCFG_SOC_SERIES_BES2700` | unknown | 9 | 4 | 45 | 0 | 0 / 9 | — | — |
| `USERSPACE_CLIENT_SUPPORT` | unknown | 12 | 16 | 28 | 0 | 0 / 29 | — | — |
| `LOSCFG_BASE_CORE_HILOG` | unknown | 6 | 39 | 0 | 0 | 0 / 6 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_SPI` | configuration | 8 | 0 | 38 | 0 | 0 / 6 | — | `adapter/khdf/linux/platform/Makefile` |
| `LOSCFG_DRIVERS_HDF_PLATFORM_SPI` | configuration | 8 | 0 | 38 | 0 | 0 / 6 | — | `adapter/khdf/liteos/platform/BUILD.gn` |
| `HDFHICOLLIE_ENABLE` | configuration | 3 | 37 | 0 | 0 | 0 / 5 | — | `adapter/uhdf2/pub_utils/BUILD.gn` |
| `LOSCFG_NET_LWIP_SACK_2_0` | unknown | 7 | 34 | 0 | 0 | 0 / 4 | — | — |
| `MIPI_TX_DEBUG` | unknown | 6 | 34 | 0 | 0 | 0 / 6 | — | — |
| `LOSCFG_DRIVERS_HDF_USB_PNP_NOTIFY` | configuration | 7 | 32 | 0 | 0 | 0 / 6 | — | `adapter/khdf/liteos/model/usb/host/BUILD.gn` |
| `CONFIG_ARCH_SPRD` | configuration | 3 | 0 | 30 | 0 | 0 / 3 | — | `adapter/khdf/linux/model/display/Makefile` |
| `LOSCFG_PLATFORM_STM32MP157` | unknown | 3 | 0 | 30 | 0 | 0 / 3 | — | — |
| `KERNEL_SERVER_SUPPORT` | unknown | 6 | 1 | 28 | 0 | 0 / 23 | — | — |
| `CONFIG_DRIVERS_HDF_NETDEV_EXT` | unknown | 1 | 21 | 0 | 0 | 0 / 1 | — | — |
| `MIPI_CSI_TEST_SELF` | unknown | 2 | 21 | 0 | 0 | 0 / 2 | — | — |
| `_LINUX_USER_` | configuration | 5 | 6 | 15 | 0 | 0 / 5 | — | `adapter/uhdf/test/unittest/platform/BUILD.gn` |
| `LOSCFG_KERNEL_SMP` | unknown | 3 | 17 | 0 | 0 | 0 / 3 | — | — |
| `WATCHDOG_TEST_BARK_RESET` | unknown | 1 | 16 | 0 | 0 | 0 / 1 | — | — |

### Unknown names

98 names nothing in the checkout accounts for; 86 of them control an always-excluded arm. Top 40 by attributable lines:

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `LOSCFG_FS_VFS` | unknown | 3 | 502 | 0 | 0 | 0 / 3 | — | — |
| `CONFIG_DRIVERS_HDF_IMX8MM_ETHERNET` | unknown | 9 | 331 | 0 | 0 | 0 / 75 | — | — |
| `CONFIG_USER_I2C_SUPPORT` | unknown | 2 | 0 | 124 | 0 | 0 / 2 | — | — |
| `LOSCFG_USER_I2C_SUPPORT` | unknown | 2 | 0 | 124 | 0 | 0 / 2 | — | — |
| `__KERNEL__` | unknown | 31 | 84 | 7 | 0 | 0 / 149 | — | — |
| `LOSCFG_SOC_SERIES_BES2600` | unknown | 9 | 31 | 45 | 0 | 0 / 9 | — | — |
| `LOSCFG_KERNEL_TRACE` | unknown | 7 | 66 | 0 | 0 | 0 / 6 | — | — |
| `WITH_SELINUX` | unknown | 7 | 56 | 0 | 0 | 0 / 7 | — | — |
| `LOSCFG_SOC_SERIES_BES2700` | unknown | 9 | 4 | 45 | 0 | 0 / 9 | — | — |
| `USERSPACE_CLIENT_SUPPORT` | unknown | 12 | 16 | 28 | 0 | 0 / 29 | — | — |
| `LOSCFG_BASE_CORE_HILOG` | unknown | 6 | 39 | 0 | 0 | 0 / 6 | — | — |
| `LOSCFG_NET_LWIP_SACK_2_0` | unknown | 7 | 34 | 0 | 0 | 0 / 4 | — | — |
| `MIPI_TX_DEBUG` | unknown | 6 | 34 | 0 | 0 | 0 / 6 | — | — |
| `LOSCFG_PLATFORM_STM32MP157` | unknown | 3 | 0 | 30 | 0 | 0 / 3 | — | — |
| `KERNEL_SERVER_SUPPORT` | unknown | 6 | 1 | 28 | 0 | 0 / 23 | — | — |
| `CONFIG_DRIVERS_HDF_NETDEV_EXT` | unknown | 1 | 21 | 0 | 0 | 0 / 1 | — | — |
| `MIPI_CSI_TEST_SELF` | unknown | 2 | 21 | 0 | 0 | 0 / 2 | — | — |
| `LOSCFG_KERNEL_SMP` | unknown | 3 | 17 | 0 | 0 | 0 / 3 | — | — |
| `WATCHDOG_TEST_BARK_RESET` | unknown | 1 | 16 | 0 | 0 | 0 / 1 | — | — |
| `__LITEOS_M__` | unknown | 15 | 15 | 0 | 0 | 0 / 105 | — | — |
| `MIPI_DSI_TEST_ON_INIT` | unknown | 2 | 11 | 0 | 0 | 0 / 2 | — | — |
| `USB_ENABLE_HISYSEVENT` | unknown | 2 | 9 | 0 | 0 | 0 / 2 | — | — |
| `PLATFORM_TEST_ON_INIT` | unknown | 1 | 8 | 0 | 0 | 0 / 1 | — | — |
| `_PRE_WLAN_FEATURE_LWIP_IPV6_AUTOCONFIG` | unknown | 2 | 8 | 0 | 0 | 0 / 2 | — | — |
| `DMA_CORE_DEBUG` | unknown | 3 | 7 | 0 | 0 | 0 / 3 | — | — |
| `CACHE_ALIGNED_SIZE` | unknown | 6 | 6 | 0 | 0 | 0 / 6 | — | — |
| `CDC_ECM` | unknown | 5 | 6 | 0 | 0 | 0 / 5 | — | — |
| `CONFIG_COMPAT` | unknown | 3 | 6 | 0 | 0 | 0 / 3 | — | — |
| `ENABLE_LWIP_MONITOR` | unknown | 2 | 6 | 0 | 0 | 0 / 2 | — | — |
| `ENABLE_WLAN_PARAM_MONITOR` | unknown | 2 | 6 | 0 | 0 | 0 / 2 | — | — |
| `REGULATOR_PRINTK_OPEN` | unknown | 1 | 6 | 0 | 0 | 0 / 5 | — | — |
| `REGULATOR_TEST_PRINTK_OPEN` | unknown | 1 | 6 | 0 | 0 | 0 / 2 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_CAN` | unknown | 2 | 0 | 5 | 0 | 0 / 2 | — | — |
| `CONFIG_DRIVERS_HDF_USB_DDK_HOST` | unknown | 2 | 0 | 5 | 0 | 0 / 2 | — | — |
| `CONFIG_ARCH_MESON` | unknown | 3 | 2 | 2 | 0 | 0 / 3 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_APB` | unknown | 1 | 0 | 4 | 0 | 0 / 1 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_DMA` | unknown | 1 | 0 | 4 | 0 | 0 / 1 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_HDMI` | unknown | 1 | 0 | 4 | 0 | 0 / 1 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_I2S` | unknown | 3 | 0 | 4 | 0 | 0 / 1 | — | — |
| `CONFIG_DRIVERS_HDF_PLATFORM_I3C` | unknown | 3 | 0 | 4 | 0 | 0 / 1 | — | — |

---

## hiviewdfx_hiview

Generated from `conditional_coverage /private/tmp/corpora/hiviewdfx_hiview` with `-D` defines: (none).

| | |
|---|---:|
| Files preprocessed (translation units + headers) | 1,428 (690 + 738) |
| Headers evaluated by more than one unit | 542 |
| Source lines | 173,562 |
| Conditional chains | 1,199 |
| … include guards | 738 |
| … evaluated by at least one run | 1,166 |
| … never evaluated (inside excluded code in every run) | 33 |
| … left unterminated | 0 |
| **Lines always excluded** | **7,387 (4.3%)** |
| Lines excluded in some runs, included in others | 1 (0.0%) |
| Files with an always-excluded arm | 111 |

### Which arm is taken

| Chains (guards excluded) | Count |
|--------------------------|------:|
| Evaluated by at least one run | 428 |
| First arm never taken | 384 |
| … of which an `#else` arm was always taken | 76 |
| … of which no arm was ever taken (no `#else`, or every arm false) | 308 |
| First arm always taken | 43 |
| First arm taken in some runs, not in others | 1 |

| Always-excluded arms by directive | Arms | Lines |
|-----------------------------------|-----:|------:|
| `#if` | 98 | 738 |
| `#ifdef` | 286 | 6,573 |
| `#ifndef` | 0 | 0 |
| `#elif` | 13 | 22 |
| `#else` | 23 | 54 |

### Excluded lines by name class

| Class | Names | Chains reading them | Lines excluded, sole dependency | Lines excluded, shared | Unbound reads |
|-------|------:|--------------------:|-------------------------------:|-----------------------:|--------------:|
| configuration | 41 | 386 | 6,744 | 40 | 898 |
| unknown | 16 | 49 | 379 | 188 | 899 |
| toolchain | 7 | 42 | 62 | 180 | 2,273 |
| include-guard | 736 | 738 | 0 | 0 | 8,782 |

### Top expressions by lines always excluded

| Expression (chain as written) | Regions | Files | Lines always excluded | Lines sometimes excluded | Names (class) |
|-------------------------------|--------:|------:|----------------------:|-------------------------:|---------------|
| `#ifdef BINDER_CATCHER_ENABLE` | 16 | 7 | 1,283 | 0 | `BINDER_CATCHER_ENABLE` (configuration) |
| `#ifdef DMESG_CATCHER_ENABLE` | 9 | 5 | 671 | 0 | `DMESG_CATCHER_ENABLE` (configuration) |
| `#ifdef USAGE_CATCHER_ENABLE` | 18 | 9 | 584 | 0 | `USAGE_CATCHER_ENABLE` (configuration) |
| `#ifdef APPEVENT_PUBLISH_ENABLE / #else` | 2 | 2 | 487 | 0 | `APPEVENT_PUBLISH_ENABLE` (configuration) |
| `#if FOLD_PC_COUNT_DURATION_ENABLE` | 38 | 10 | 378 | 0 | `FOLD_PC_COUNT_DURATION_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_CPU_ENABLE / #else` | 2 | 2 | 369 | 0 | `UNIFIED_COLLECTOR_CPU_ENABLE` (configuration) |
| `#ifdef HITRACE_CATCHER_ENABLE` | 16 | 9 | 334 | 0 | `HITRACE_CATCHER_ENABLE` (configuration) |
| `#ifdef STACKTRACE_CATCHER_ENABLE` | 11 | 5 | 324 | 0 | `STACKTRACE_CATCHER_ENABLE` (configuration) |
| `#ifdef SCB_CATCHER_ENABLE` | 9 | 5 | 301 | 0 | `SCB_CATCHER_ENABLE` (configuration) |
| `#ifdef WINDOW_MANAGER_ENABLE` | 20 | 5 | 289 | 0 | `WINDOW_MANAGER_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_TRACE_ENABLE` | 18 | 5 | 264 | 0 | `UNIFIED_COLLECTOR_TRACE_ENABLE` (configuration) |
| `#ifdef MULTIMODALINPUT_INPUT_ENABLE` | 2 | 2 | 256 | 0 | `MULTIMODALINPUT_INPUT_ENABLE` (unknown) |
| `#ifdef HILOG_CATCHER_ENABLE` | 16 | 8 | 181 | 0 | `HILOG_CATCHER_ENABLE` (configuration) |
| `#ifdef OTHER_CATCHER_ENABLE` | 12 | 5 | 180 | 0 | `OTHER_CATCHER_ENABLE` (configuration) |
| `#ifdef HAS_HIPERF / #else` | 1 | 1 | 178 | 0 | `HAS_HIPERF` (configuration) |
| `#if defined(__HIVIEW_OHOS__) / #elif defined(_WIN32)` | 9 | 2 | 161 | 0 | `__HIVIEW_OHOS__` (unknown), `_WIN32` (toolchain) |
| `#if FOLD_PC_COUNT_DURATION_ENABLE / #else` | 32 | 6 | 146 | 0 | `FOLD_PC_COUNT_DURATION_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_IO_ENABLE / #else` | 1 | 1 | 113 | 0 | `UNIFIED_COLLECTOR_IO_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_MEMORY_ENABLE / #else` | 1 | 1 | 109 | 0 | `UNIFIED_COLLECTOR_MEMORY_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_GRAPHIC_ENABLE / #else` | 2 | 2 | 88 | 0 | `UNIFIED_COLLECTOR_GRAPHIC_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_THERMAL_ENABLE / #else` | 1 | 1 | 84 | 0 | `UNIFIED_COLLECTOR_THERMAL_ENABLE` (configuration) |
| `#ifdef USE_POLL` | 1 | 1 | 47 | 0 | `USE_POLL` (unknown) |
| `#ifdef UNIFIED_COLLECTOR_HILOG_ENABLE / #else` | 1 | 1 | 41 | 0 | `UNIFIED_COLLECTOR_HILOG_ENABLE` (configuration) |
| `#ifdef DEVICE_USAGE_STATISTICS_ENABLE / #else` | 1 | 1 | 34 | 0 | `DEVICE_USAGE_STATISTICS_ENABLE` (configuration) |
| `#ifndef UNIFIED_COLLECTOR_TRACE_ENABLE / #else` | 6 | 1 | 34 | 0 | `UNIFIED_COLLECTOR_TRACE_ENABLE` (configuration) |
| `#ifdef UNIFIED_COLLECTOR_GPU_ENABLE / #else` | 1 | 1 | 25 | 0 | `UNIFIED_COLLECTOR_GPU_ENABLE` (configuration) |
| `#ifdef _WIN32 / #else` | 3 | 3 | 22 | 0 | `_WIN32` (toolchain) |
| `#ifdef POWER_MANAGER_ENABLE` | 8 | 3 | 21 | 0 | `POWER_MANAGER_ENABLE` (configuration) |
| `#ifdef RESOURCE_SCHEDULE_SERVICE_ENABLE` | 6 | 2 | 21 | 0 | `RESOURCE_SCHEDULE_SERVICE_ENABLE` (configuration) |
| `#if defined(KERNELSTACK_CATCHER_ENABLE) && defined(DMESG_CATCHER_ENABLE)` | 1 | 1 | 20 | 0 | `KERNELSTACK_CATCHER_ENABLE` (configuration), `DMESG_CATCHER_ENABLE` (configuration) |
| `#ifdef TRACE_MANAGER_UNITTEST` | 6 | 4 | 20 | 0 | `TRACE_MANAGER_UNITTEST` (configuration) |
| `#ifdef WUKONG_COLLECT_GPU_INFO / #else` | 2 | 1 | 19 | 0 | `WUKONG_COLLECT_GPU_INFO` (configuration) |
| `#ifdef CATCH_TRACE_FOR_CPU_HIGH_LOAD` | 5 | 2 | 18 | 0 | `CATCH_TRACE_FOR_CPU_HIGH_LOAD` (configuration) |
| `#ifdef HAS_HIPROFILER / #else` | 1 | 1 | 18 | 0 | `HAS_HIPROFILER` (unknown) |
| `#if defined(__HIVIEW_OHOS__)` | 2 | 2 | 17 | 0 | `__HIVIEW_OHOS__` (unknown) |
| `#ifdef JSONCPP_VERSION_STRING / #else` | 4 | 2 | 17 | 0 | `JSONCPP_VERSION_STRING` (unknown) |
| `#ifdef USAGE_FOLD_STAT_ENABLE / #else` | 1 | 1 | 15 | 0 | `USAGE_FOLD_STAT_ENABLE` (configuration) |
| `#ifdef __HIVIEW_OHOS__ / #elif defined(_WIN32) / #else` | 1 | 1 | 15 | 0 | `__HIVIEW_OHOS__` (unknown), `_WIN32` (toolchain) |
| `#ifdef NOT_BUILD_FOR_OHOS_SDK` | 5 | 2 | 14 | 0 | `NOT_BUILD_FOR_OHOS_SDK` (unknown) |
| `#if defined(__aarch64__)` | 1 | 1 | 12 | 0 | `__aarch64__` (toolchain) |

### Names, derived view

Lines are apportioned as described above: *sole* when the name is the only one the chain reads, *shared* otherwise (a shared line is listed under every name of its chain).

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `BINDER_CATCHER_ENABLE` | configuration | 16 | 1,283 | 0 | 0 | 0 / 20 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `DMESG_CATCHER_ENABLE` | configuration | 10 | 671 | 20 | 0 | 0 / 14 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `USAGE_CATCHER_ENABLE` | configuration | 18 | 584 | 0 | 0 | 0 / 24 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `FOLD_PC_COUNT_DURATION_ENABLE` | configuration | 71 | 524 | 0 | 0 | 0 / 198 | — | `plugins/usage_event_report/BUILD.gn` |
| `APPEVENT_PUBLISH_ENABLE` | configuration | 3 | 495 | 0 | 0 | 0 / 3 | — | `base/event_publish/test/BUILD.gn` |
| `UNIFIED_COLLECTOR_CPU_ENABLE` | configuration | 9 | 377 | 0 | 0 | 0 / 9 | — | `framework/native/unified_collection/BUILD.gn` |
| `HITRACE_CATCHER_ENABLE` | configuration | 20 | 334 | 0 | 0 | 0 / 23 | — | `plugins/eventlogger/BUILD.gn` |
| `STACKTRACE_CATCHER_ENABLE` | configuration | 11 | 324 | 0 | 0 | 0 / 16 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `SCB_CATCHER_ENABLE` | configuration | 9 | 301 | 0 | 0 | 0 / 15 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `UNIFIED_COLLECTOR_TRACE_ENABLE` | configuration | 24 | 298 | 0 | 0 | 0 / 24 | — | `framework/native/unified_collection/BUILD.gn` |
| `WINDOW_MANAGER_ENABLE` | configuration | 21 | 294 | 0 | 0 | 0 / 27 | — | `plugins/eventlogger/BUILD.gn` |
| `MULTIMODALINPUT_INPUT_ENABLE` | unknown | 2 | 256 | 0 | 0 | 0 / 2 | — | — |
| `_WIN32` | toolchain | 24 | 36 | 176 | 0 | 0 / 1,710 | — | — |
| `__HIVIEW_OHOS__` | unknown | 12 | 17 | 176 | 0 | 0 / 739 | — | — |
| `HAS_HIPERF` | configuration | 16 | 187 | 0 | 0 | 0 / 9 | — | `framework/native/unified_collection/decorator/test/BUILD.gn` |
| `HILOG_CATCHER_ENABLE` | configuration | 16 | 181 | 0 | 0 | 0 / 22 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `OTHER_CATCHER_ENABLE` | configuration | 12 | 180 | 0 | 0 | 0 / 18 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `UNIFIED_COLLECTOR_IO_ENABLE` | configuration | 8 | 121 | 0 | 0 | 0 / 8 | — | `framework/native/unified_collection/BUILD.gn` |
| `UNIFIED_COLLECTOR_MEMORY_ENABLE` | configuration | 9 | 117 | 0 | 0 | 0 / 8 | — | `framework/native/unified_collection/BUILD.gn` |
| `UNIFIED_COLLECTOR_GRAPHIC_ENABLE` | configuration | 5 | 91 | 0 | 0 | 0 / 5 | — | `framework/native/unified_collection/BUILD.gn` |
| `UNIFIED_COLLECTOR_THERMAL_ENABLE` | configuration | 4 | 87 | 0 | 0 | 0 / 4 | — | `framework/native/unified_collection/BUILD.gn` |
| `UNIFIED_COLLECTOR_HILOG_ENABLE` | configuration | 8 | 50 | 0 | 0 | 0 / 8 | — | `framework/native/unified_collection/BUILD.gn` |
| `USE_POLL` | unknown | 8 | 47 | 0 | 0 | 0 / 1 | — | — |
| `DEVICE_USAGE_STATISTICS_ENABLE` | configuration | 4 | 39 | 0 | 0 | 0 / 4 | — | `plugins/usage_event_report/service/BUILD.gn` |
| `UNIFIED_COLLECTOR_GPU_ENABLE` | configuration | 8 | 33 | 0 | 0 | 0 / 8 | — | `framework/native/unified_collection/BUILD.gn` |
| `TRACE_MANAGER_UNITTEST` | configuration | 8 | 22 | 0 | 0 | 0 / 82 | — | `framework/native/unified_collection/trace_manager/test/BUILD.gn` |
| `KERNELSTACK_CATCHER_ENABLE` | configuration | 8 | 1 | 20 | 0 | 0 / 2 | — | `plugins/eventlogger/log_catcher/BUILD.gn` |
| `POWER_MANAGER_ENABLE` | configuration | 8 | 21 | 0 | 0 | 0 / 8 | — | `plugins/unified_collector/BUILD.gn` |
| `RESOURCE_SCHEDULE_SERVICE_ENABLE` | configuration | 6 | 21 | 0 | 0 | 0 / 14 | — | `plugins/performance/perfmonitor/BUILD.gn` |
| `HAS_HIPROFILER` | unknown | 3 | 20 | 0 | 0 | 0 / 6 | — | — |
| `WUKONG_COLLECT_GPU_INFO` | configuration | 2 | 19 | 0 | 0 | 0 / 2 | — | `framework/native/unified_collection/BUILD.gn` |
| `CATCH_TRACE_FOR_CPU_HIGH_LOAD` | configuration | 5 | 18 | 0 | 0 | 0 / 6 | — | `plugins/unified_collector/BUILD.gn` |
| `JSONCPP_VERSION_STRING` | unknown | 4 | 17 | 0 | 0 | 0 / 4 | — | — |
| `USAGE_FOLD_STAT_ENABLE` | configuration | 2 | 17 | 0 | 0 | 0 / 8 | — | `plugins/usage_event_report/BUILD.gn` |
| `NOT_BUILD_FOR_OHOS_SDK` | unknown | 5 | 14 | 0 | 0 | 0 / 8 | — | — |
| `__aarch64__` | toolchain | 3 | 12 | 2 | 0 | 0 / 21 | — | — |
| `UNITTEST` | configuration | 4 | 12 | 0 | 0 | 0 / 4 | — | `plugins/faultlogger/service/BUILD.gn` |
| `__LP64__` | toolchain | 2 | 12 | 0 | 0 | 0 / 2 | — | — |
| `UNIT_TEST` | configuration | 2 | 8 | 0 | 0 | 0 / 2 | — | `plugins/faultlogger/service/BUILD.gn` |
| `TRACE_STRATEGY_UNITTEST` | configuration | 8 | 7 | 0 | 0 | 0 / 8 | — | `framework/native/unified_collection/collector/impl/trace/test/BUILD.gn` |

### Unknown names

16 names nothing in the checkout accounts for; 13 of them control an always-excluded arm. Top 16 by attributable lines:

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `MULTIMODALINPUT_INPUT_ENABLE` | unknown | 2 | 256 | 0 | 0 | 0 / 2 | — | — |
| `__HIVIEW_OHOS__` | unknown | 12 | 17 | 176 | 0 | 0 / 739 | — | — |
| `USE_POLL` | unknown | 8 | 47 | 0 | 0 | 0 / 1 | — | — |
| `HAS_HIPROFILER` | unknown | 3 | 20 | 0 | 0 | 0 / 6 | — | — |
| `JSONCPP_VERSION_STRING` | unknown | 4 | 17 | 0 | 0 | 0 / 4 | — | — |
| `NOT_BUILD_FOR_OHOS_SDK` | unknown | 5 | 14 | 0 | 0 | 0 / 8 | — | — |
| `THERMAL_MANAGER_ENABLE` | unknown | 3 | 5 | 0 | 0 | 0 / 2 | — | — |
| `__BIG_ENDIAN` | unknown | 2 | 0 | 4 | 0 | 0 / 0 | — | — |
| `__BYTE_ORDER` | unknown | 2 | 0 | 4 | 0 | 0 / 66 | — | — |
| `__LITTLE_ENDIAN` | unknown | 2 | 0 | 4 | 0 | 0 / 66 | — | — |
| `BBOX_USERSPACE` | unknown | 1 | 1 | 0 | 0 | 0 / 2 | — | — |
| `ENABLE_DIRTY_EVENT_CLEAR` | unknown | 1 | 1 | 0 | 0 | 0 / 1 | — | — |
| `KERNEL_DEVICE_BUFFER` | unknown | 1 | 1 | 0 | 0 | 0 / 1 | — | — |
| `DUMP_STACK_IN_PROCESS` | unknown | 1 | 0 | 0 | 0 | 0 / 0 | — | — |
| `EPOLL_CLOEXEC` | unknown | 1 | 0 | 0 | 0 | 0 / 0 | — | — |
| `TEST_LOCAL_SRC` | unknown | 1 | 0 | 0 | 0 | 0 / 1 | — | — |

---

## multimedia_camera_framework

Generated from `conditional_coverage /private/tmp/corpora/multimedia_camera_framework` with `-D` defines: (none).

| | |
|---|---:|
| Files preprocessed (translation units + headers) | 1,593 (744 + 849) |
| Headers evaluated by more than one unit | 562 |
| Source lines | 403,098 |
| Conditional chains | 1,257 |
| … include guards | 849 |
| … evaluated by at least one run | 1,257 |
| … never evaluated (inside excluded code in every run) | 0 |
| … left unterminated | 0 |
| **Lines always excluded** | **4,991 (1.2%)** |
| Lines excluded in some runs, included in others | 35 (0.0%) |
| Files with an always-excluded arm | 94 |

### Which arm is taken

| Chains (guards excluded) | Count |
|--------------------------|------:|
| Evaluated by at least one run | 408 |
| First arm never taken | 360 |
| … of which an `#else` arm was always taken | 33 |
| … of which no arm was ever taken (no `#else`, or every arm false) | 327 |
| First arm always taken | 48 |
| First arm taken in some runs, not in others | 0 |

| Always-excluded arms by directive | Arms | Lines |
|-----------------------------------|-----:|------:|
| `#if` | 2 | 2 |
| `#ifdef` | 358 | 4,980 |
| `#ifndef` | 0 | 0 |
| `#elif` | 0 | 0 |
| `#else` | 3 | 9 |

### Excluded lines by name class

| Class | Names | Chains reading them | Lines excluded, sole dependency | Lines excluded, shared | Unbound reads |
|-------|------:|--------------------:|-------------------------------:|-----------------------:|--------------:|
| configuration | 22 | 360 | 4,976 | 0 | 3,553 |
| unknown | 4 | 6 | 13 | 0 | 760 |
| toolchain | 3 | 44 | 0 | 4 | 130 |
| include-guard | 830 | 849 | 0 | 0 | 21,299 |

### Top expressions by lines always excluded

| Expression (chain as written) | Regions | Files | Lines always excluded | Lines sometimes excluded | Names (class) |
|-------------------------------|--------:|------:|----------------------:|-------------------------:|---------------|
| `#ifdef CAMERA_CAPTURE_YUV` | 84 | 40 | 2,090 | 0 | `CAMERA_CAPTURE_YUV` (configuration) |
| `#ifdef CAMERA_MOVING_PHOTO` | 68 | 22 | 893 | 0 | `CAMERA_MOVING_PHOTO` (configuration) |
| `#ifdef CAMERA_MOVIE_FILE` | 21 | 8 | 303 | 0 | `CAMERA_MOVIE_FILE` (configuration) |
| `#ifdef HOOK_CAMERA_OPERATOR` | 23 | 8 | 220 | 0 | `HOOK_CAMERA_OPERATOR` (configuration) |
| `#ifdef CAMERA_LIVE_SCENE_RECOGNITION` | 16 | 9 | 213 | 0 | `CAMERA_LIVE_SCENE_RECOGNITION` (configuration) |
| `#ifdef NOTIFICATION_ENABLE` | 15 | 7 | 203 | 0 | `NOTIFICATION_ENABLE` (configuration) |
| `#ifdef CAMERA_FRAMEWORK_FEATURE_MEDIA_STREAM` | 19 | 10 | 194 | 0 | `CAMERA_FRAMEWORK_FEATURE_MEDIA_STREAM` (configuration) |
| `#ifdef COMPATIBILITY_CONFIG_CENTER_ENABLE` | 9 | 2 | 179 | 0 | `COMPATIBILITY_CONFIG_CENTER_ENABLE` (configuration) |
| `#ifdef CAMERA_USE_SENSOR` | 20 | 11 | 158 | 0 | `CAMERA_USE_SENSOR` (configuration) |
| `#ifdef CAMERA_MOVING_PHOTO / #else` | 9 | 4 | 116 | 0 | `CAMERA_MOVING_PHOTO` (configuration) |
| `#ifdef CAMERA_CAPTURE_YUV / #else` | 8 | 6 | 115 | 0 | `CAMERA_CAPTURE_YUV` (configuration) |
| `#ifdef MEMMGR_OVERRID` | 18 | 6 | 77 | 0 | `MEMMGR_OVERRID` (configuration) |
| `#ifdef CAMERA_USE_BATTERY` | 5 | 2 | 36 | 0 | `CAMERA_USE_BATTERY` (configuration) |
| `#ifdef CAMERA_FRAMEWORK_FEATURE_MEDIA_STREAM / #else` | 4 | 4 | 31 | 0 | `CAMERA_FRAMEWORK_FEATURE_MEDIA_STREAM` (configuration) |
| `#ifdef DEVICE_MANAGER` | 4 | 2 | 30 | 0 | `DEVICE_MANAGER` (configuration) |
| `#ifdef CAMERA_SERVICE_PRIORITY` | 3 | 2 | 18 | 0 | `CAMERA_SERVICE_PRIORITY` (configuration) |
| `#ifdef CAMERA_MOVIE_FILE / #else` | 2 | 2 | 16 | 0 | `CAMERA_MOVIE_FILE` (configuration) |
| `#ifdef CAMERA_USE_IMAGE_EFFECT / #else` | 4 | 1 | 16 | 0 | `CAMERA_USE_IMAGE_EFFECT` (configuration) |
| `#ifdef CAMERA_XCOMPONENT_TOAST` | 5 | 1 | 16 | 0 | `CAMERA_XCOMPONENT_TOAST` (configuration) |
| `#ifdef CAMERA_USE_POWER` | 4 | 3 | 15 | 0 | `CAMERA_USE_POWER` (configuration) |
| `#ifdef CAMERA_ROTATE_PARAM_UPDATE` | 3 | 1 | 9 | 0 | `CAMERA_ROTATE_PARAM_UPDATE` (configuration) |
| `#ifdef CAMERA_USE_IMAGE_EFFECT` | 3 | 1 | 8 | 0 | `CAMERA_USE_IMAGE_EFFECT` (configuration) |
| `#ifndef IS_RELEASE_VERSION / #else` | 2 | 2 | 8 | 0 | `IS_RELEASE_VERSION` (unknown) |
| `#ifdef COMPATIBILITY_CONFIG_CENTER_ENABLE / #else` | 1 | 1 | 5 | 0 | `COMPATIBILITY_CONFIG_CENTER_ENABLE` (configuration) |
| `#ifdef PICKER_PRE_REQUIRE_MEM` | 1 | 1 | 5 | 0 | `PICKER_PRE_REQUIRE_MEM` (configuration) |
| `#ifdef CAMERA_USE_THERMAL` | 2 | 1 | 4 | 0 | `CAMERA_USE_THERMAL` (configuration) |
| `#ifdef CONFIG_USE_JEMALLOC_DFX_INTF` | 1 | 1 | 3 | 0 | `CONFIG_USE_JEMALLOC_DFX_INTF` (unknown) |
| `#if (defined(__aarch64__) \|\| defined(__x86_64__)) / #else` | 2 | 2 | 2 | 0 | `__aarch64__` (toolchain), `__x86_64__` (toolchain) |
| `#ifdef CAMERA_EMULATOR / #else` | 1 | 1 | 2 | 0 | `CAMERA_EMULATOR` (configuration) |
| `#ifdef HICOLLIE_ENABLE` | 2 | 1 | 2 | 0 | `HICOLLIE_ENABLE` (configuration) |
| `#if !defined(CROSS_PLATFORM) / #else` | 1 | 1 | 1 | 0 | `CROSS_PLATFORM` (unknown) |
| `#ifdef CAMERA_DISABLE_ZOOM_RATIO_FOR_AUDIO / #else` | 1 | 1 | 1 | 0 | `CAMERA_DISABLE_ZOOM_RATIO_FOR_AUDIO` (unknown) |
| `#ifdef HICOLLIE_ENABLE / #else` | 1 | 1 | 1 | 0 | `HICOLLIE_ENABLE` (configuration) |
| `#ifdef NAPI_ASSERT` | 1 | 1 | 1 | 0 | `NAPI_ASSERT` (configuration) |

### Names, derived view

Lines are apportioned as described above: *sole* when the name is the only one the chain reads, *shared* otherwise (a shared line is listed under every name of its chain).

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `CAMERA_CAPTURE_YUV` | configuration | 95 | 2,205 | 0 | 0 | 0 / 1,126 | — | `common/BUILD.gn` |
| `CAMERA_MOVING_PHOTO` | configuration | 77 | 1,009 | 0 | 0 | 0 / 802 | — | `frameworks/native/camera/base/BUILD.gn` |
| `CAMERA_MOVIE_FILE` | configuration | 23 | 319 | 0 | 0 | 0 / 105 | — | `frameworks/native/camera/base/BUILD.gn` |
| `CAMERA_FRAMEWORK_FEATURE_MEDIA_STREAM` | configuration | 23 | 225 | 0 | 0 | 0 / 205 | — | `frameworks/native/camera/base/BUILD.gn` |
| `HOOK_CAMERA_OPERATOR` | configuration | 23 | 220 | 0 | 0 | 0 / 56 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_LIVE_SCENE_RECOGNITION` | configuration | 16 | 213 | 0 | 0 | 0 / 99 | — | `frameworks/native/camera/test/unittest/camera_service/BUILD.gn` |
| `NOTIFICATION_ENABLE` | configuration | 15 | 203 | 0 | 0 | 0 / 153 | — | `frameworks/native/camera/test/unittest/camera_service/BUILD.gn` |
| `COMPATIBILITY_CONFIG_CENTER_ENABLE` | configuration | 10 | 184 | 0 | 0 | 0 / 132 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_USE_SENSOR` | configuration | 20 | 158 | 0 | 0 | 0 / 609 | — | `frameworks/native/camera/base/BUILD.gn` |
| `MEMMGR_OVERRID` | configuration | 18 | 77 | 0 | 0 | 0 / 110 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_USE_BATTERY` | configuration | 5 | 36 | 0 | 0 | 0 / 5 | — | `services/deferred_processing_service/BUILD.gn` |
| `DEVICE_MANAGER` | configuration | 4 | 30 | 0 | 0 | 0 / 37 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_USE_IMAGE_EFFECT` | configuration | 7 | 24 | 0 | 0 | 0 / 7 | — | `mediastream/BUILD.gn` |
| `CAMERA_SERVICE_PRIORITY` | configuration | 3 | 18 | 0 | 0 | 0 / 36 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_XCOMPONENT_TOAST` | configuration | 5 | 16 | 0 | 0 | 0 / 5 | — | `services/camera_service/BUILD.gn` |
| `CAMERA_USE_POWER` | configuration | 4 | 15 | 0 | 0 | 0 / 5 | — | `common/BUILD.gn` |
| `CAMERA_ROTATE_PARAM_UPDATE` | configuration | 3 | 9 | 0 | 0 | 0 / 3 | — | `services/camera_service/BUILD.gn` |
| `IS_RELEASE_VERSION` | unknown | 2 | 8 | 0 | 0 | 0 / 691 | — | — |
| `PICKER_PRE_REQUIRE_MEM` | configuration | 1 | 5 | 0 | 0 | 0 / 1 | — | `interfaces/kits/js/camera_napi/BUILD.gn` |
| `CAMERA_USE_THERMAL` | configuration | 2 | 4 | 0 | 0 | 0 / 2 | — | `services/deferred_processing_service/BUILD.gn` |
| `CONFIG_USE_JEMALLOC_DFX_INTF` | unknown | 1 | 3 | 0 | 0 | 0 / 66 | — | — |
| `HICOLLIE_ENABLE` | configuration | 4 | 3 | 0 | 0 | 0 / 4 | `test/fuzztest/cameradevice_fuzzer/camera_device_fuzzer.cpp:263` | `common/BUILD.gn` |
| `CAMERA_EMULATOR` | configuration | 1 | 2 | 0 | 0 | 0 / 1 | — | `frameworks/native/camera/base/BUILD.gn` |
| `__aarch64__` | toolchain | 2 | 0 | 2 | 0 | 0 / 65 | — | — |
| `__x86_64__` | toolchain | 2 | 0 | 2 | 0 | 0 / 65 | — | — |
| `CAMERA_DISABLE_ZOOM_RATIO_FOR_AUDIO` | unknown | 1 | 1 | 0 | 0 | 0 / 1 | — | — |
| `CROSS_PLATFORM` | unknown | 2 | 1 | 0 | 0 | 0 / 2 | — | — |
| `NAPI_ASSERT` | configuration | 1 | 1 | 0 | 0 | 0 / 50 | `interfaces/kits/js/camera_napi/include/camera_napi_utils.h:38` | — |
| `CAMERA_XML_NODE_H` | include-guard | 2 | 0 | 0 | 35 | 1 / 70 | `common/utils/camera_xml_parser.h:16` (+1) | — |

### Unknown names

4 names nothing in the checkout accounts for; 4 of them control an always-excluded arm. Top 4 by attributable lines:

| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |
|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|
| `IS_RELEASE_VERSION` | unknown | 2 | 8 | 0 | 0 | 0 / 691 | — | — |
| `CONFIG_USE_JEMALLOC_DFX_INTF` | unknown | 1 | 3 | 0 | 0 | 0 / 66 | — | — |
| `CAMERA_DISABLE_ZOOM_RATIO_FOR_AUDIO` | unknown | 1 | 1 | 0 | 0 | 0 / 1 | — | — |
| `CROSS_PLATFORM` | unknown | 2 | 1 | 0 | 0 | 0 / 2 | — | — |

---
