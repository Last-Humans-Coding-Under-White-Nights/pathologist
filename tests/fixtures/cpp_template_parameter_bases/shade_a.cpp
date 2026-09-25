#include "local.h"
// Seen by the include graph only: the unit never includes it, so its
// file-local `Shade` must not replace the linked one below.
#if 0
#include "shade_anon.h"
#endif

// A linked template sharing its name with other files' file-local ones.
template <class T> struct Shade : T {};
struct ShadeA : Shade<ILocal> {};
