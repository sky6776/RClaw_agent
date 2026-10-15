# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# ---------------------------------------------------------------------------
# Rust 侧经 JNI 按名调用（FindClass/GetStaticMethodID/call_method）的桥类。
# R8 看不到这些调用点，缺 keep 会在 release 构建剥离/改名方法——实证 v2.7.1/
# v2.8.0：installUpdate 等被剥，升级报「调用 NativeBridge.installUpdate 失败:
# Java exception was thrown」；runScript 被改名，skill 脚本执行全坏。
# release.yml 的「Verify JNI bridge survived R8」步骤兜底校验。
# ---------------------------------------------------------------------------
-keep class ai.rclaw.app.NativeBridge { *; }
-keep class ai.rclaw.app.PythonRuntime { *; }