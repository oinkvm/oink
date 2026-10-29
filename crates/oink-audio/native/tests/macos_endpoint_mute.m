// HAL notifications/control writes are all stubbed. No device or audio IO.
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#include <assert.h>
static OSStatus fake_get(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32 *,void *);
static OSStatus fake_set(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32,const void *);
static OSStatus fake_settable(AudioObjectID,const AudioObjectPropertyAddress *,Boolean *);
static OSStatus fake_add(AudioObjectID,const AudioObjectPropertyAddress *,dispatch_queue_t,AudioObjectPropertyListenerBlock);
static OSStatus fake_remove(AudioObjectID,const AudioObjectPropertyAddress *,dispatch_queue_t,AudioObjectPropertyListenerBlock);
#define AudioObjectGetPropertyData fake_get
#define AudioObjectSetPropertyData fake_set
#define AudioObjectIsPropertySettable fake_settable
#define AudioObjectAddPropertyListenerBlock fake_add
#define AudioObjectRemovePropertyListenerBlock fake_remove
#include "../macos_mute.m"
static AudioDeviceID mapped=100, default_device=100;
static float level=0.375f;
static bool is_muted;
static unsigned writes,subscriptions,fail_subscription;
static AudioObjectPropertyListenerBlock blocks[3];
static dispatch_queue_t queues[3];
static void notify(unsigned slot,AudioObjectPropertySelector selector){
    AudioObjectPropertyAddress a={selector,slot? kAudioObjectPropertyScopeGlobal:kAudioObjectPropertyScopeOutput,0};
    AudioObjectPropertyListenerBlock block=blocks[slot];
    dispatch_async(queues[slot], ^{block(1,&a);});
}
static void flush(void *raw){OinkMuteObserver *o=(__bridge OinkMuteObserver *)raw;dispatch_sync(o->queue,^{});}
static OSStatus fake_get(AudioObjectID object,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 *n,void *v){
    (void)qs;(void)q;(void)n;
    switch(a->mSelector){
        case kAudioHardwarePropertyTranslateUIDToDevice:*(AudioDeviceID *)v=mapped;return noErr;
        case kAudioHardwarePropertyDefaultOutputDevice:*(AudioDeviceID *)v=default_device;return noErr;
        case kAudioDevicePropertyVolumeScalar:assert(object==100);*(float *)v=level;return noErr;
        case kAudioDevicePropertyMute:assert(object==100);*(UInt32 *)v=is_muted;return noErr;
        default:return kAudioHardwareUnknownPropertyError;
    }
}
static OSStatus fake_set(AudioObjectID object,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 n,const void *v){
    (void)qs;(void)q;(void)n;assert(object==100&&a->mSelector==kAudioDevicePropertyMute);
    is_muted=*(const UInt32 *)v;writes++;notify(0,kAudioDevicePropertyMute);return noErr;
}
static OSStatus fake_settable(AudioObjectID object,const AudioObjectPropertyAddress *a,Boolean *value){
    assert(object==100&&a->mSelector==kAudioDevicePropertyMute);*value=true;return noErr;
}
static OSStatus fake_add(AudioObjectID object,const AudioObjectPropertyAddress *a,dispatch_queue_t queue,AudioObjectPropertyListenerBlock block){
    (void)object;(void)a;
    if(++subscriptions==fail_subscription)return kAudioHardwareIllegalOperationError;
    blocks[subscriptions-1]=block;queues[subscriptions-1]=queue;return noErr;
}
static OSStatus fake_remove(AudioObjectID object,const AudioObjectPropertyAddress *a,dispatch_queue_t queue,AudioObjectPropertyListenerBlock block){
    (void)object;(void)a;(void)queue;(void)block;return noErr;
}
static void *observer(void){
    mapped=default_device=100;is_muted=false;writes=subscriptions=0;level=0.375f;
    char error[512]={0};void *o=oink_audio_endpoint_open("test-output",error,sizeof(error));assert(o&&subscriptions==3);return o;
}
static void close_observer(void *o){oink_audio_endpoint_close(o);for(unsigned i=0;i<3;i++){blocks[i]=nil;queues[i]=nil;}}
static void wake_owner(void *context){ ++*(unsigned *)context; }
int main(void){@autoreleasepool{
    char error[512]={0};float volume;bool muted;
    void *o=observer();
    unsigned wakes=0;
    oink_audio_endpoint_on_change(o,wake_owner,&wakes);
    assert(!oink_audio_endpoint_set_mute(o,true,false,error,sizeof(error)));
    assert(oink_audio_endpoint_flags(o)==0&&writes==1);
    assert(oink_audio_endpoint_read(o,&volume,&muted,error,sizeof(error))==1&&muted&&volume==0.375f);
    notify(0,kAudioDevicePropertyMute);flush(o);
    assert(oink_audio_endpoint_flags(o)==0); // Duplicate same-state echo.
    // Own echo never acts like a user volume key. A real level event revokes
    // the full lease; release-only may undo mute without touching that level.
    unsigned prior_wakes=wakes;
    level=0.75f;notify(0,kAudioDevicePropertyVolumeScalar);flush(o);
    assert(oink_audio_endpoint_flags(o)==1 && wakes == prior_wakes + 1);
    assert(oink_audio_endpoint_set_mute(o,false,false,error,sizeof(error)));
    assert(!oink_audio_endpoint_set_mute(o,false,true,error,sizeof(error)));
    assert(!is_muted&&level==0.75f&&writes==2);close_observer(o);
    o=observer();assert(!oink_audio_endpoint_set_mute(o,true,false,error,sizeof(error)));
    is_muted=false;notify(0,kAudioDevicePropertyMute);flush(o);
    is_muted=true;notify(0,kAudioDevicePropertyMute);flush(o);
    assert((oink_audio_endpoint_flags(o)&3)==3);
    assert(oink_audio_endpoint_set_mute(o,false,true,error,sizeof(error))&&is_muted&&writes==1);
    close_observer(o);
    o=observer();notify(1,kAudioHardwarePropertyDefaultOutputDevice);flush(o);assert(!oink_audio_endpoint_flags(o));
    default_device=200;notify(1,kAudioHardwarePropertyDefaultOutputDevice);flush(o);
    default_device=100;notify(1,kAudioHardwarePropertyDefaultOutputDevice);flush(o);
    assert(oink_audio_endpoint_flags(o)==4&&writes==0); // A -> B -> A stays retired.
    mapped=200;assert(oink_audio_endpoint_read(o,&volume,&muted,error,sizeof(error))==0);
    assert(oink_audio_endpoint_set_mute(o,true,false,error,sizeof(error))&&writes==0);close_observer(o);
    o=observer();notify(2,kAudioHardwarePropertyServiceRestarted);flush(o);
    assert(oink_audio_endpoint_read(o,&volume,&muted,error,sizeof(error))==0);
    assert(oink_audio_endpoint_set_mute(o,true,false,error,sizeof(error))&&writes==0);close_observer(o);
    // Failed listener setup tears down only registered listeners, before any write.
    subscriptions=0;writes=0;fail_subscription=2;
    assert(!oink_audio_endpoint_open("test-output",error,sizeof(error))&&writes==0);
    for(unsigned i=0;i<3;i++){blocks[i]=nil;queues[i]=nil;}
    puts("endpoint mute: own echo, user override, old UID, default switch and reset; no HAL/audio IO");
}return 0;}
