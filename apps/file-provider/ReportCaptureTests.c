// Isolated subprocess fixture; never loaded into the installed extension host.
#include <stdint.h>
#include <signal.h>
int oink_report_arm(const char*,uint64_t,const unsigned char*,const uint16_t*,uint32_t,uint8_t);
void oink_report_disarm(void);
int main(int argc,char**argv) {
    if(argc!=3)return 2;
    unsigned char capability[32]={0};uint16_t version[3]={1,0,0};
    if(!oink_report_arm(argv[1],1,capability,version,1,14))return 3;
    if(argv[2][0]=='0')oink_report_disarm();
    raise(SIGABRT);return 4;
}
