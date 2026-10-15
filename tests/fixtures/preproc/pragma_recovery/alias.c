#define DO_PRAGMA _Pragma
void Before(void);
DO_PRAGMA("message(\"x\")")
void AfterAlias(void) { Before(); }
