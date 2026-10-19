// The real bridge with every HAL mutation/read replaced by deterministic stubs.
// No tap, device, IO, or audio service is created by this test.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#include <assert.h>
#include <stdio.h>
static OSStatus fake_get(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32 *,void *);
static OSStatus fake_size(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32 *);
static OSStatus fake_set(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32,const void *);
static OSStatus fake_listener(AudioObjectID,const AudioObjectPropertyAddress *,AudioObjectPropertyListenerProc,void *);
static OSStatus fake_tap(CATapDescription *,AudioObjectID *);
static OSStatus fake_aggregate(CFDictionaryRef,AudioObjectID *);
static OSStatus fake_io(AudioObjectID,AudioDeviceIOProc,void *,AudioDeviceIOProcID *);
static OSStatus fake_start(AudioObjectID,AudioDeviceIOProcID);
static OSStatus fake_stop(AudioObjectID,AudioDeviceIOProcID);
static OSStatus fake_unregister(AudioObjectID,AudioDeviceIOProcID);
static OSStatus fake_destroy(AudioObjectID);
#define AudioObjectGetPropertyData fake_get
#define AudioObjectGetPropertyDataSize fake_size
#define AudioObjectSetPropertyData fake_set
#define AudioObjectAddPropertyListener fake_listener
#define AudioHardwareCreateProcessTap fake_tap
#define AudioHardwareCreateAggregateDevice fake_aggregate
#define AudioDeviceCreateIOProcID fake_io
#define AudioDeviceStart fake_start
#define AudioDeviceStop fake_stop
#define AudioDeviceDestroyIOProcID fake_unregister
#define AudioHardwareDestroyProcessTap fake_destroy
#define AudioHardwareDestroyAggregateDevice fake_destroy
#include "../macos.m"
static unsigned taps,starts;
static AudioStreamBasicDescription format(void){return (AudioStreamBasicDescription){.mSampleRate=48000,.mFormatID=kAudioFormatLinearPCM,.mFormatFlags=kAudioFormatFlagIsFloat,.mBitsPerChannel=32,.mChannelsPerFrame=2};}
static OSStatus fake_size(AudioObjectID o,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 *n){
 (void)qs;(void)q;
 switch(a->mSelector){
 case kAudioDevicePropertyStreams:*n=(o==100&&a->mScope==kAudioObjectPropertyScopeInput)?0:sizeof(AudioStreamID);return noErr;
 case kAudioDevicePropertyStreamConfiguration:*n=sizeof(AudioBufferList);return noErr;
 default:return kAudioHardwareUnknownPropertyError;
 }
}
static OSStatus fake_get(AudioObjectID o,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 *n,void *v){
 (void)qs;(void)q;
 switch(a->mSelector){
 case kAudioHardwarePropertyTranslatePIDToProcessObject:*(AudioObjectID *)v=10;return noErr;
 case kAudioHardwarePropertyDefaultOutputDevice:*(AudioDeviceID *)v=100;return noErr;
 case kAudioDevicePropertyDeviceIsAlive:*(UInt32 *)v=1;return noErr;
 case kAudioDevicePropertyBufferFrameSize:*(UInt32 *)v=128;return noErr;
 case kAudioDevicePropertyNominalSampleRate:*(Float64 *)v=48000;return noErr;
 case kAudioDevicePropertyDeviceUID:*(CFStringRef *)v=CFRetain(CFSTR("synthetic-output"));return noErr;
 case kAudioDevicePropertyStreams:*(AudioStreamID *)v=400;*n=sizeof(AudioStreamID);return noErr;
 case kAudioDevicePropertyStreamConfiguration:{AudioBufferList *b=v;b->mNumberBuffers=1;b->mBuffers[0]=(AudioBuffer){.mNumberChannels=2};return noErr;}
 case kAudioDevicePropertyIOProcStreamUsage:{AudioHardwareIOProcStreamUsage *u=v;u->mNumberStreams=1;u->mStreamIsOn[0]=0;return noErr;}
 case kAudioTapPropertyFormat:case kAudioStreamPropertyVirtualFormat:*(AudioStreamBasicDescription *)v=format();return noErr;
 default:return kAudioHardwareUnknownPropertyError;
 }
}
static OSStatus fake_set(AudioObjectID o,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 n,const void *v){
 (void)o;(void)qs;(void)q;(void)n;(void)v;
 assert(a->mSelector==kAudioDevicePropertyIOProcStreamUsage);return noErr;
}
static OSStatus fake_listener(AudioObjectID o,const AudioObjectPropertyAddress *a,AudioObjectPropertyListenerProc p,void *c){(void)o;(void)a;(void)p;(void)c;return noErr;}
static OSStatus fake_tap(CATapDescription *d,AudioObjectID *o){assert(d.muteBehavior==CATapUnmuted);*o=200;taps++;return noErr;}
static OSStatus fake_aggregate(CFDictionaryRef d,AudioObjectID *o){(void)d;*o=300;return noErr;}
static OSStatus fake_io(AudioObjectID d,AudioDeviceIOProc f,void *c,AudioDeviceIOProcID *o){(void)d;(void)c;*o=(AudioDeviceIOProcID)f;return noErr;}
static OSStatus fake_start(AudioObjectID d,AudioDeviceIOProcID i){(void)d;(void)i;starts++;return noErr;}
static OSStatus fake_stop(AudioObjectID d,AudioDeviceIOProcID i){(void)d;(void)i;return noErr;}
static OSStatus fake_unregister(AudioObjectID d,AudioDeviceIOProcID i){(void)d;(void)i;return noErr;}
static OSStatus fake_destroy(AudioObjectID d){(void)d;return noErr;}
static void scenario(bool mute,uint32_t mode){
 char error[1024]={0};double rate=0;
 void *capture=oink_audio_capture_start(mute,UINT32_MAX,mode,NULL,false,true,false,false,&rate,error,sizeof(error));
 assert(capture&&rate==48000&&error[0]==0);
 assert(oink_audio_capture_check(capture,error,sizeof(error))==0);
 assert(oink_audio_capture_stop(capture,error,sizeof(error))==noErr);
}
int main(void){@autoreleasepool{scenario(true,1);scenario(false,1);scenario(true,0);assert(taps==3&&starts==3);puts("process tap is always unmuted; endpoint lease owns source silence; no HAL/audio IO");}return 0;}
