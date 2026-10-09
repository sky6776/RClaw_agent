// RClaw whisper JNI 包装：暴露 NativeBridge.whisperTranscribe(model, wav, lang)。
// 输入要求 16kHz 单声道/双声道 16-bit PCM WAV（由 Java 侧 MediaCodec 解码产出）。
// 错误不经 JNI 异常（跨层会丢消息）：一律返回 "error:<原因>"，成功返回转写文本。
// 阻塞式推理，调用方必须在工作线程。
#include <jni.h>
#include <string>
#include <vector>
#include <cstdint>
#include <cstring>

#include "whisper.h"

namespace {

// 极简 RIFF WAV 读取：只接受 16-bit PCM；双声道取平均折成单声道。
// 返回 false 时把原因写进 error 供上层抛 Java 异常。
bool read_wav_16k_mono(const char * path, std::vector<float> & out, std::string & error) {
    FILE * f = fopen(path, "rb");
    if (!f) { error = "打不开 WAV 文件"; return false; }

    auto fail = [&](const char * msg) { fclose(f); error = msg; return false; };

    unsigned char hdr[12];
    if (fread(hdr, 1, 12, f) != 12 || memcmp(hdr, "RIFF", 4) || memcmp(hdr + 8, "WAVE", 4)) {
        return fail("不是 RIFF/WAVE 文件");
    }

    int format = 0, channels = 0, rate = 0, bits = 0;
    std::vector<int16_t> pcm;
    bool have_fmt = false, have_data = false;

    while (true) {
        unsigned char ch[8];
        size_t got = fread(ch, 1, 8, f);
        if (got < 8) break;
        uint32_t size = (uint32_t) ch[4] | ((uint32_t) ch[5] << 8) |
                        ((uint32_t) ch[6] << 16) | ((uint32_t) ch[7] << 24);
        if (memcmp(ch, "fmt ", 4) == 0) {
            unsigned char fmt[16];
            if (size < 16 || fread(fmt, 1, 16, f) != 16) return fail("fmt 块损坏");
            format  = fmt[0] | (fmt[1] << 8);
            channels = fmt[2] | (fmt[3] << 8);
            rate    = fmt[4] | (fmt[5] << 8) | (fmt[6] << 16) | (fmt[7] << 24);
            bits    = fmt[14] | (fmt[15] << 8);
            have_fmt = true;
            if (size > 16) fseek(f, size - 16, SEEK_CUR);
        } else if (memcmp(ch, "data", 4) == 0) {
            if (!have_fmt) return fail("data 块出现在 fmt 之前");
            if (format != 1) return fail("只支持 16-bit PCM WAV");
            if (bits != 16) return fail("只支持 16-bit 位深");
            if (rate != 16000) {
                char msg[64];
                snprintf(msg, sizeof(msg), "采样率需为 16000Hz，实际 %dHz", rate);
                return fail(msg);
            }
            if (channels < 1 || channels > 2) return fail("只支持单/双声道");
            std::vector<unsigned char> raw(size);
            if (fread(raw.data(), 1, size, f) != size) return fail("data 块被截断");
            size_t frames = size / 2 / channels;
            pcm.resize(frames);
            for (size_t i = 0; i < frames; ++i) {
                if (channels == 1) {
                    pcm[i] = (int16_t)(raw[i * 2] | (raw[i * 2 + 1] << 8));
                } else {
                    int l = (int16_t)(raw[i * 4]     | (raw[i * 4 + 1] << 8));
                    int r = (int16_t)(raw[i * 4 + 2] | (raw[i * 4 + 3] << 8));
                    pcm[i] = (int16_t)((l + r) / 2);
                }
            }
            have_data = true;
            if (size % 2) fseek(f, 1, SEEK_CUR); // RIFF 块按 2 字节对齐
        } else {
            fseek(f, size + (size % 2), SEEK_CUR);
        }
    }
    fclose(f);
    if (!have_data) { error = "WAV 缺少 data 块"; return false; }

    out.resize(pcm.size());
    for (size_t i = 0; i < pcm.size(); ++i) out[i] = (float) pcm[i] / 32768.0f;
    return true;
}

std::string jstring_to_std(JNIEnv * env, jstring s) {
    if (!s) return {};
    const char * p = env->GetStringUTFChars(s, nullptr);
    std::string out(p ? p : "");
    if (p) env->ReleaseStringUTFChars(s, p);
    return out;
}

} // namespace

extern "C" JNIEXPORT jstring JNICALL
Java_ai_rclaw_app_NativeBridge_whisperTranscribe(
        JNIEnv * env, jobject /*thiz*/, jstring model_path, jstring wav_path, jstring language) {

    std::string model = jstring_to_std(env, model_path);
    std::string wav = jstring_to_std(env, wav_path);
    std::string lang = jstring_to_std(env, language);
    if (lang.empty()) lang = "auto";

    std::vector<float> pcmf32;
    std::string error;
    if (!read_wav_16k_mono(wav.c_str(), pcmf32, error)) {
        return env->NewStringUTF(("error:音频读取失败: " + error).c_str());
    }

    whisper_context_params cparams = whisper_context_default_params();
    cparams.use_gpu = false; // vivo 无可用 GPU 后端，纯 CPU。

    whisper_context * ctx = whisper_init_from_file_with_params(model.c_str(), cparams);
    if (!ctx) {
        return env->NewStringUTF("error:whisper 模型加载失败（检查模型文件是否完整）");
    }

    whisper_full_params wparams = whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
    wparams.n_threads = 4; // 骁龙 675：2 大核 + 6 小核，4 线程折中。
    wparams.language = lang.c_str();
    wparams.translate = false;
    wparams.print_progress = false;
    wparams.print_special = false;
    wparams.print_realtime = false;
    wparams.no_context = true;      // 短片段防上下文复读幻听。
    wparams.suppress_blank = true;
    wparams.single_segment = false;
    wparams.greedy.best_of = 1;     // 移动端速度优先。

    std::string result;
    if (whisper_full(ctx, wparams, pcmf32.data(), (int) pcmf32.size()) != 0) {
        whisper_free(ctx);
        return env->NewStringUTF("error:whisper 推理失败");
    }
    const int n = whisper_full_n_segments(ctx);
    for (int i = 0; i < n; ++i) {
        const char * text = whisper_full_get_segment_text(ctx, i);
        if (text) result += text;
    }
    whisper_free(ctx);

    // 首尾去空白（whisper 常给段首补空格）。
    size_t b = result.find_first_not_of(" \t\r\n");
    size_t e = result.find_last_not_of(" \t\r\n");
    if (b == std::string::npos) return env->NewStringUTF("");
    return env->NewStringUTF(result.substr(b, e - b + 1).c_str());
}
