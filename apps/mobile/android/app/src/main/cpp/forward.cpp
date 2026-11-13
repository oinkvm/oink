#include <jni.h>
#include <cstdint>
#include "oink_mobile_audio.h"

extern "C" JNIEXPORT jlong JNICALL
Java_com_hachiwii_oink_audio_NativeForward_create(JNIEnv*, jobject, jint rate, jint channels, jlong generation) {
    return reinterpret_cast<jlong>(oink_audio_forward_create(rate, channels, generation));
}
extern "C" JNIEXPORT jint JNICALL
Java_com_hachiwii_oink_audio_NativeForward_push(JNIEnv* env, jobject, jlong handle, jshortArray samples,
                                              jint frames, jlong host_ns, jboolean discontinuity) {
    if (!handle || frames <= 0 || frames > 4096 || env->GetArrayLength(samples) != frames * 2) return 0;
    // This is the serial pipe worker, never an AudioRecord/AAudio realtime callback.
    jshort* pcm = env->GetShortArrayElements(samples, nullptr);
    if (!pcm) return 0;
    auto accepted = oink_audio_forward_push_i16(reinterpret_cast<OinkAudioForward*>(handle),
        reinterpret_cast<const int16_t*>(pcm), frames, host_ns, discontinuity ? 1 : 0);
    env->ReleaseShortArrayElements(samples, pcm, JNI_ABORT);
    return static_cast<jint>(accepted);
}
extern "C" JNIEXPORT void JNICALL
Java_com_hachiwii_oink_audio_NativeForward_revoke(JNIEnv*, jobject, jlong handle) {
    if (handle) oink_audio_forward_revoke(reinterpret_cast<OinkAudioForward*>(handle));
}
extern "C" JNIEXPORT void JNICALL
Java_com_hachiwii_oink_audio_NativeForward_destroy(JNIEnv*, jobject, jlong handle) {
    if (handle) oink_audio_forward_destroy(reinterpret_cast<OinkAudioForward*>(handle));
}
