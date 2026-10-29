#include <stdlib.h>
#include <stdint.h>
#include <malloc/malloc.h>
static __thread uint64_t count;
static __thread int armed;
void oink_heap_audit_begin(void) { count = 0; armed = 1; }
uint64_t oink_heap_audit_end(void) { armed = 0; return count; }
static void *audit_malloc(size_t n) { if (armed) count++; return malloc(n); }
static void *audit_calloc(size_t n,size_t s) { if (armed) count++; return calloc(n,s); }
static void *audit_realloc(void *p,size_t n) { if (armed) count++; return realloc(p,n); }
static void audit_free(void *p) { if (armed && p) count++; free(p); }
static void *audit_zone_malloc(malloc_zone_t *z,size_t n) { if (armed) count++; return malloc_zone_malloc(z,n); }
static void *audit_zone_calloc(malloc_zone_t *z,size_t n,size_t s) { if (armed) count++; return malloc_zone_calloc(z,n,s); }
static void *audit_zone_realloc(malloc_zone_t *z,void *p,size_t n) { if (armed) count++; return malloc_zone_realloc(z,p,n); }
static void audit_zone_free(malloc_zone_t *z,void *p) { if (armed && p) count++; malloc_zone_free(z,p); }
#define INTERPOSE(replacement, original) __attribute__((used)) static struct {const void *a;const void *b;} pair_##original __attribute__((section("__DATA,__interpose"))) = {(const void *)(uintptr_t)&replacement,(const void *)(uintptr_t)&original};
INTERPOSE(audit_malloc,malloc) INTERPOSE(audit_calloc,calloc) INTERPOSE(audit_realloc,realloc) INTERPOSE(audit_free,free)
INTERPOSE(audit_zone_malloc,malloc_zone_malloc) INTERPOSE(audit_zone_calloc,malloc_zone_calloc) INTERPOSE(audit_zone_realloc,malloc_zone_realloc) INTERPOSE(audit_zone_free,malloc_zone_free)
