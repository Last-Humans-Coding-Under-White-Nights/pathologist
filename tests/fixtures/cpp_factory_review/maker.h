// A cached header imports widget.h as types only: the constructor lookup
// here is empty without proving Widget has no constructor, so a factory
// keeps an unresolved site for the TU merge to resolve, as `new` does.
#include "widget.h"
void Handler();
inline void header_new() { auto w = new Widget(Handler); }
inline void header_shared() { auto w = std::make_shared<Widget>(Handler); }
inline void header_unique() { auto w = std::make_unique<Widget>(Handler); }
inline void header_sptr() { auto w = OHOS::sptr<Widget>::MakeSptr(Handler); }
