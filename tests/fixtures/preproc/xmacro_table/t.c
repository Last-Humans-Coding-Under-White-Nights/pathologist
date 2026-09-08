/* An X-macro table: the same file included once per `#define` of its entry
   macro. Suppressing the second `#include` loses every `use_*` and both
   call edges, silently (#56). */
#define ENTRY(n) void fn_##n(void) {}
#include "list.def"
#undef ENTRY

#define ENTRY(n) void use_##n(void) { fn_##n(); }
#include "list.def"
#undef ENTRY
