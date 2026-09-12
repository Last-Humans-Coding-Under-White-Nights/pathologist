#ifndef CPP_SMART_PTR_HOLDER_H
#define CPP_SMART_PTR_HOLDER_H

#include "wrapper.h"

class Widget {
public:
    virtual int Draw();
};

// The wrapper-typed member lives in a *different* header from the wrapper.
class WidgetBox {
public:
    int DrawHeld();

private:
    Handle<Widget> held_;
};

class MissingWidgetBox {
public:
    missing<Widget> held;
};

template<class T> class HeaderNoArrow {};

#endif
