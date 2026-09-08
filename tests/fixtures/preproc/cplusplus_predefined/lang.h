#ifndef LANG_H
#define LANG_H
#ifdef __cplusplus
#define LANG_CALL() cxx_path()
#else
#define LANG_CALL() c_path()
#endif
#endif
