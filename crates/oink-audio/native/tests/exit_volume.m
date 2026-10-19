#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#include <assert.h>
#import <ApplicationServices/ApplicationServices.h>
static void fake_post(CGEventTapLocation,CGEventRef);
static bool fake_preflight(void);
#define CGEventPost fake_post
#define CGPreflightPostEventAccess fake_preflight
static OSStatus fake_get(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32 *,void *);
static OSStatus fake_set(AudioObjectID,const AudioObjectPropertyAddress *,UInt32,const void *,UInt32,const void *);
static Boolean fake_has(AudioObjectID,const AudioObjectPropertyAddress *);
static OSStatus fake_settable(AudioObjectID,const AudioObjectPropertyAddress *,Boolean *);
static OSStatus fake_add(AudioObjectID,const AudioObjectPropertyAddress *,dispatch_queue_t,AudioObjectPropertyListenerBlock);
static OSStatus fake_remove(AudioObjectID,const AudioObjectPropertyAddress *,dispatch_queue_t,AudioObjectPropertyListenerBlock);
#define AudioObjectGetPropertyData fake_get
#define AudioObjectSetPropertyData fake_set
#define AudioObjectHasProperty fake_has
#define AudioObjectIsPropertySettable fake_settable
#define AudioObjectAddPropertyListenerBlock fake_add
#define AudioObjectRemovePropertyListenerBlock fake_remove
#include "../exit_volume.m"
static AudioDeviceID mapped=100, default_output=200;
static unsigned posts;
static int expected_key;
static CGEventFlags expected_flags;
static bool post_allowed=true;
static bool fake_preflight(void){return post_allowed;}
static void fake_post(CGEventTapLocation tap,CGEventRef event){
    assert(tap==kCGHIDEventTap);
    assert(CGEventGetType(event)==14);
    assert(CGEventGetIntegerValueField(event,83)==8);
    assert(CGEventGetIntegerValueField(event,kCGEventSourceUserData)==0x4F494E4B);
    assert(CGEventGetIntegerValueField(event,150)==-1);
    int64_t data=CGEventGetIntegerValueField(event,149);
    assert((data>>16)==expected_key);
    assert((CGEventGetFlags(event)&0xffff0000)==expected_flags);
    assert(((data>>8)&0xff)==(posts%2?0xB:0xA));
    posts++;
}
static float level=0.375f;
static bool is_muted,unsupported,deny;
static unsigned writes,subscriptions,removals,fail_subscription,wakes;
static void wake_owner(void *context){assert(context==&wakes);wakes++;}
static AudioObjectPropertyListenerBlock blocks[3];
static dispatch_queue_t queues[3];
static void notify(unsigned slot,AudioObjectPropertySelector selector){
    AudioObjectPropertyAddress a={selector,slot? kAudioObjectPropertyScopeGlobal:kAudioObjectPropertyScopeOutput,0};
    AudioObjectPropertyListenerBlock block=blocks[slot];
    dispatch_async(queues[slot], ^{block(1,&a);});
}
static void flush(void *raw){OinkExitVolume *o=(__bridge OinkExitVolume *)raw;dispatch_sync(o->queue,^{});}
static OSStatus fake_get(AudioObjectID object,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 *n,void *v){
    (void)qs;(void)q;(void)n;
    switch(a->mSelector){
        case kAudioHardwarePropertyDefaultOutputDevice:*(AudioDeviceID *)v=default_output;return noErr;
        case kAudioHardwarePropertyTranslateUIDToDevice:*(AudioDeviceID *)v=mapped;return noErr;
        case kAudioDevicePropertyDeviceIsAlive:*(UInt32 *)v=1;return noErr;
        case kAudioDevicePropertyVolumeScalar:assert(object==100);*(float *)v=level;return noErr;
        case kAudioDevicePropertyMute:assert(object==100);*(UInt32 *)v=is_muted;return noErr;
        default:assert(!"unexpected device property access");return kAudioHardwareUnknownPropertyError;
    }
}
static OSStatus fake_set(AudioObjectID object,const AudioObjectPropertyAddress *a,UInt32 qs,const void *q,UInt32 n,const void *v){
    (void)qs;(void)q;(void)n;assert(object==100);
    if(deny)return kAudioDevicePermissionsError;
    if(a->mSelector==kAudioDevicePropertyMute)is_muted=*(const UInt32 *)v;
    else {assert(a->mSelector==kAudioDevicePropertyVolumeScalar);level=*(const float *)v;}
    writes++;notify(0,a->mSelector);return noErr;
}
static Boolean fake_has(AudioObjectID object,const AudioObjectPropertyAddress *a){(void)a;assert(object==100);return !unsupported;}
static OSStatus fake_settable(AudioObjectID object,const AudioObjectPropertyAddress *a,Boolean *value){
    (void)a;assert(object==100);*value=true;return noErr;
}
static OSStatus fake_add(AudioObjectID object,const AudioObjectPropertyAddress *a,dispatch_queue_t queue,AudioObjectPropertyListenerBlock block){
    (void)object;(void)a;
    if(++subscriptions==fail_subscription)return kAudioHardwareIllegalOperationError;
    blocks[subscriptions-1]=block;queues[subscriptions-1]=queue;return noErr;
}
static OSStatus fake_remove(AudioObjectID object,const AudioObjectPropertyAddress *a,dispatch_queue_t queue,AudioObjectPropertyListenerBlock block){
    (void)object;(void)a;(void)queue;(void)block;removals++;return noErr;
}
static void *observer(void){
    mapped=100;is_muted=unsupported=deny=false;writes=subscriptions=removals=wakes=0;level=0.375f;
    int result=0;int32_t native=0;void *o=oink_exit_volume_open("synthetic-output",&result,&native,wake_owner,&wakes);assert(o&&result==0&&subscriptions==3&&writes==0);return o;
}
static void close_observer(void *o){oink_exit_volume_close(o);for(unsigned i=0;i<3;i++){blocks[i]=nil;queues[i]=nil;}}
int main(void){@autoreleasepool{
    float volume;bool muted;int32_t native=0;int result=0;
    void *o=observer();
    assert(oink_exit_volume_read(o,&volume,&muted,&native)==0&&volume==0.375f&&!muted);
    assert(!oink_exit_volume_changed(o));
    level=0.75f;notify(0,kAudioDevicePropertyVolumeScalar);flush(o);
    assert(oink_exit_volume_changed(o)&&writes==0&&wakes==1);
    assert(oink_exit_volume_read(o,&volume,&muted,&native)==0&&volume==0.75f);
    assert(oink_exit_volume_set(o,0.5f,true,&native)==0&&writes==2);flush(o);
    assert(oink_exit_volume_read(o,&volume,&muted,&native)==0&&volume==0.5f&&muted);
    deny=true;assert(oink_exit_volume_set(o,0.4f,false,&native)==3&&writes==2);
    mapped=200;assert(oink_exit_volume_set(o,0.4f,false,&native)==2&&writes==2);
    mapped=100;notify(2,kAudioHardwarePropertyServiceRestarted);flush(o);
    assert(oink_exit_volume_set(o,0.4f,false,&native)==2&&writes==2);
    close_observer(o);assert(removals==3);
    o=observer();
    assert(oink_exit_volume_key(o,1,0,&native)==0&&level==0.4375f&&!is_muted);flush(o);
    assert(oink_exit_volume_key(o,-1,0,&native)==0&&level==0.375f&&!is_muted);flush(o);
    assert(oink_exit_volume_key(o,0,0,&native)==0&&is_muted);flush(o);
    assert(writes==6&&wakes==6);
    assert(oink_exit_volume_key(o,1,0x06,&native)==0&&level==0.390625f&&!is_muted);flush(o);
    assert(writes==8&&wakes==8);close_observer(o);
    o=observer();default_output=100;
    assert(oink_exit_volume_key(o,1,0,&native)==0);
    assert(posts==2&&writes==0&&level==0.375f);
    expected_key=1;expected_flags=kCGEventFlagMaskShift|kCGEventFlagMaskAlternate;
    assert(oink_exit_volume_key(o,-1,0x60,&native)==0&&posts==4&&writes==0);
    expected_key=7;expected_flags=0;
    assert(oink_exit_volume_key(o,0,0,&native)==0&&posts==6&&writes==0);
    post_allowed=false;assert(oink_exit_volume_key(o,1,0,&native)==3&&posts==6&&writes==0);
    post_allowed=true;
    mapped=200;assert(oink_exit_volume_key(o,1,0,&native)==2&&posts==6);
    mapped=100;
    level=0.5f;notify(0,kAudioDevicePropertyVolumeScalar);flush(o);
    assert(oink_exit_volume_changed(o)&&wakes==1);
    assert(oink_exit_volume_read(o,&volume,&muted,&native)==0&&volume==0.5f);
    close_observer(o);
    unsupported=true;assert(!oink_exit_volume_open("synthetic-output",&result,&native,NULL,NULL)&&result==1);
    unsupported=false;subscriptions=removals=0;fail_subscription=2;
    assert(!oink_exit_volume_open("synthetic-output",&result,&native,NULL,NULL)&&result==4&&removals==1);
    for(unsigned i=0;i<3;i++){blocks[i]=nil;queues[i]=nil;}
    return 0;
}}
