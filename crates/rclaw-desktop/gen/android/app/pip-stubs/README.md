# Android dependency markers

These local distributions satisfy dependencies declared by `yfinance` and `akshare` which
aren't used by the stock-history path. Their upstream artifacts either have no Android wheel,
or contain glibc, curl-impersonate, or V8 binaries which cannot be loaded by Android's Bionic
runtime.

RClaw deliberately uses yfinance's documented requests backend and loads only AKShare's
requests-based stock history module on Android. The full upstream Python packages remain in
the APK; unsupported native backends are not bundled or silently emulated.
