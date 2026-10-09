import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("com.chaquo.python") version "17.0.0"
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

android {
    // 与 v0.2.0 工程一致的钉版（v1.5 review P2-3）：minSdk 26 起 WebView 自动更新。
    compileSdk = 36
    ndkVersion = "29.0.14206865"
    namespace = "ai.rclaw.app"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "ai.rclaw.app"
        minSdk = 26
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.androidName", "2.0.0")
        testInstrumentationRunner = "ai.rclaw.app.SkillScriptInstrumentation"

        ndk {
            // 单 ABI：与 v0.2.0 一致，Chaquopy 轮子与 Rust .so 都只带 arm64。
            abiFilters += "arm64-v8a"
        }
    }
    sourceSets {
        getByName("main") {
            // 内置 skill 资产（syncBundledSkills 产物）作为额外 assets 目录。
            assets.directories.add(
                layout.buildDirectory.dir("generated/rclawAssets").get().asFile.absolutePath,
            )
        }
    }
    androidResources {
        // whisper 模型（ggml 权重）不压缩：压缩无收益还拖慢打包与首读。
        noCompress += "bin"
    }
    // Release 签名：keystore 放 app 目录（不进 git）且 RCLAW_KEYSTORE_PASSWORD
    // 环境变量非空时自动启用；口令不入库，缺任一则退回 debug 签名。
    val releaseKeystoreFile = file("rclaw-release.keystore")
    val releaseKeystorePassword = System.getenv("RCLAW_KEYSTORE_PASSWORD")
    val hasReleaseSigning = releaseKeystoreFile.exists() && !releaseKeystorePassword.isNullOrBlank()
    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = releaseKeystoreFile
                storePassword = releaseKeystorePassword
                keyAlias = "rclaw"
                keyPassword = releaseKeystorePassword
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
            }
        }
        getByName("release") {
            // 全新克隆没有本机 keystore/口令（均不进 git）：退回 debug 签名，
            // 保证 release 包编译即可安装（目标：git 拉下来编译完直接能用）。
            signingConfig = if (hasReleaseSigning) {
                signingConfigs.getByName("release")
            } else {
                signingConfigs.getByName("debug")
            }
            optimization {
               enable = true
            }
            proguardFiles(
                *fileTree(".") {
                  include("**/*.pro")
                  exclude("build/**")
                }.files.toTypedArray()
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
    }
    lint {
        disable += "ChromeOsAbiSupport"
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

rust {
    rootDirRel = "../../../"
}

chaquopy {
    defaultConfig {
        version = "3.12"
        val configuredPython = providers.environmentVariable("RCLAW_BUILD_PYTHON").orNull
        if (configuredPython.isNullOrBlank()) {
            buildPython("py", "-3.12")
        } else {
            buildPython(configuredPython)
        }
        pip {
            // Android uses requests instead of curl_cffi, and the stock skill only needs
            // AKShare's HTTP history module. These metadata-only distributions prevent pip
            // from selecting glibc/V8 wheels which cannot run on Android's Bionic runtime.
            // 元数据 stub 已预打包为 wheel（pip-stubs/wheels/，宿主侧 pip wheel 产出；
            // 构建环境的 pip 解析不到 build backend（setuptools），装 wheel 无此依赖）。
            install("./pip-stubs/wheels/akracer-0.0.14-py3-none-any.whl")
            install("./pip-stubs/wheels/curl_cffi-0.15.0-py3-none-any.whl")
            install("./pip-stubs/wheels/jsonpath-0.82.2-py3-none-any.whl")
            install("./pip-stubs/wheels/mini_racer-0.14.1-py3-none-any.whl")
            install("./pip-stubs/wheels/py_mini_racer-0.6.0-py3-none-any.whl")
            install("requests>=2.31,<3")
            install("Pillow>=10,<12")
            install("numpy==1.26.2")
            install("pandas==2.1.3")
            install("matplotlib==3.8.2")
            install("fpdf2>=2.7")
            install("python-docx>=1.0")
            install("python-pptx>=1.0")
            install("markdown>=3.6")
            install("bleach>=6.1")
            install("mplfinance==0.12.10b0")
            install("tushare>=1.4")
            install("yfinance==1.5.1")
            install("akshare==1.18.64")
            // D2：PDF 文本提取走纯 Python pypdf（fitz 垫片已删除）。
            install("pypdf>=4,<7")
        }
    }
}

// 内置 skill 资产：从仓库 skills/ 同步（D1 五个降级包已整体删除，无需 exclude 特例）。
val bundledAssets = layout.buildDirectory.dir("generated/rclawAssets")

val syncBundledSkills by tasks.registering(Sync::class) {
    val repositoryRoot = rootProject.projectDir.parentFile.parentFile.parentFile.parentFile
    // 凭据叠加层与仓库 skills/ 同名时以后者为准：INCLUDE 让后声明的 from 覆盖先前的。
    duplicatesStrategy = DuplicatesStrategy.INCLUDE
    from(repositoryRoot.resolve("skills"))
    // 本机凭据叠加层：仓库里的 key 模板（ark_key.toml 等）入库时保持空值，
    // 真实 key 放 conf/skills（本机与仓库同级的目录，在 git 仓库之外，key
    // 物理上进不了 git）。目录存在时把 key 文件与各 skill 的 config 覆盖进
    // 内置资产——构建出的 APK 自带可用 key；目录不存在（如全新克隆）则跳过，
    // 打包行为不变。
    val credentialOverlay = listOf(
        repositoryRoot.parentFile.resolve("conf/skills"),
        repositoryRoot.resolve("conf/skills"),
    ).firstOrNull { it.isDirectory }
    if (credentialOverlay != null) {
        from(credentialOverlay) {
            include(
                "ark_key.toml",
                "happyhorse_key.toml",
                "*/config.toml",
                "*/.env",
                "*/config.env",
                "*/config.json",
            )
        }
    }
    exclude(
        "**/setup.sh",
        "**/setup.ps1",
        "**/run.sh",
        "**/run.ps1",
        "**/sync.sh",
        "**/*.mjs",
        "**/*.js",
        "**/package.json",
        "**/package-lock.json",
        "**/requirements*.txt",
        "**/last-report.json",
        "**/last-run.json",
        "**/reports/**",
        "**/venv/**",
        "**/node_modules/**",
        "**/__pycache__/**",
        "**/*.pyc",
        "**/.git/**",
        "**/tmp/**",
    )
    into(bundledAssets.map { it.dir("rclaw-skills") })
}

tasks.named("preBuild").configure {
    dependsOn(syncBundledSkills)
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = file("tauri.build.gradle.kts"))
