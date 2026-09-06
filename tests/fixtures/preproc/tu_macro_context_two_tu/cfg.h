#ifndef CFG_H
#define CFG_H
#ifdef USE_FAST
void impl(void) { fast_path(); }
#else
void impl(void) { slow_path(); }
#endif
#endif
