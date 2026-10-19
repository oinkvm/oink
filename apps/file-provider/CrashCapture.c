// Local-only fixed numeric crash capsule. No allocation, unwinding, runtime
// labels, memory dump, formatting, logging, or HTTPS in the signal handler.
#include <stdint.h>
#include <stdatomic.h>
#include <signal.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <time.h>
#include <sys/stat.h>

static atomic_flag busy = ATOMIC_FLAG_INIT;
static _Atomic int active = 0;
static int capsule_fd = -1;
static unsigned char template[128];
static const int signals[] = { SIGABRT, SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP };
static struct sigaction previous[6];
static unsigned char installed[6];
static void put64(unsigned char *p, uint64_t n) { for (int i=0;i<8;i++) p[i]=(unsigned char)(n>>(8*i)); }
static void put32(unsigned char *p, uint32_t n) { for (int i=0;i<4;i++) p[i]=(unsigned char)(n>>(8*i)); }
static void fault(int signal) {
    if (!atomic_flag_test_and_set_explicit(&busy,memory_order_acquire)) {
        if (atomic_load_explicit(&active,memory_order_acquire) && capsule_fd>=0) {
            unsigned char record[128];
            for (int i=0;i<128;i++) record[i]=template[i];
            record[17]=(signal==SIGILL || signal==SIGTRAP)?2:1;
            put32(record+20,(uint32_t)signal);
            struct timespec now;
            if (clock_gettime(CLOCK_REALTIME,&now)==0) {
                put64(record+32,(uint64_t)now.tv_sec);
                uint64_t check=0;
                for (int i=0;i<120;i++) check=((check<<7)|(check>>57))^record[i];
                put64(record+120,check);
                (void)pwrite(capsule_fd,record,sizeof(record),0);
            }
        }
        atomic_flag_clear_explicit(&busy,memory_order_release);
    }
    // Preserve platform termination and OS .ips generation. Never suppress a
    // crash or intercept a process-global handler owned by the extension host.
    for (int i=0;i<6;i++) if (signals[i]==signal && installed[i]) {
        sigaction(signal,&previous[i],0); installed[i]=0; break;
    }
    raise(signal);
}
int oink_report_arm(const char *path,uint64_t generation,const unsigned char *capability,
                    const uint16_t *version,uint32_t build,uint8_t os_major) {
    if (!generation || atomic_flag_test_and_set_explicit(&busy,memory_order_acquire)) return 0;
    atomic_store_explicit(&active,0,memory_order_release);
    if (capsule_fd>=0) close(capsule_fd);
    capsule_fd=open(path,O_CREAT|O_TRUNC|O_WRONLY|O_NOFOLLOW|O_CLOEXEC,0600);
    if (capsule_fd<0 || ftruncate(capsule_fd,128)!=0) {
        if(capsule_fd>=0) close(capsule_fd);capsule_fd=-1;
        atomic_flag_clear_explicit(&busy,memory_order_release);return 0;
    }
    memset(template,0,sizeof(template));memcpy(template,"OINKCR03",8);put64(template+8,generation);
    template[16]=3;memcpy(template+40,capability,32);
    for(int i=0;i<3;i++){template[72+i*2]=(unsigned char)version[i];template[73+i*2]=(unsigned char)(version[i]>>8);}
    put32(template+78,build);template[82]=3;template[83]=os_major;
    for(int i=0;i<6;i++) if(!installed[i]) {
        struct sigaction old;
        if(sigaction(signals[i],0,&old)==0 && old.sa_handler==SIG_DFL) {
            struct sigaction action;memset(&action,0,sizeof(action));sigemptyset(&action.sa_mask);action.sa_handler=fault;
            if(sigaction(signals[i],&action,&previous[i])==0) installed[i]=1;
        }
    }
    atomic_store_explicit(&active,1,memory_order_release);
    atomic_flag_clear_explicit(&busy,memory_order_release);return 1;
}
void oink_report_disarm(void) {
    atomic_store_explicit(&active,0,memory_order_release);
    if(atomic_flag_test_and_set_explicit(&busy,memory_order_acquire)) return;
    if(capsule_fd>=0) close(capsule_fd);capsule_fd=-1;
    for(int i=0;i<6;i++) if(installed[i]) {
        struct sigaction current;
        if(sigaction(signals[i],0,&current)==0 && current.sa_handler==fault) sigaction(signals[i],&previous[i],0);
        installed[i]=0;
    }
    atomic_flag_clear_explicit(&busy,memory_order_release);
}
