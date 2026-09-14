// Arguments of a method call bind to the method's own parameters, past the
// implicit `this` (#93).
#include "remote.hpp"

void OnDot() {}
void OnVar() {}
void OnArrow() {}
void OnFunctor() {}
void OnFieldFunctor() {}
void OnImplicit() {}
void OnThisArrow() {}
void OnQualified() {}
void OnRemote() {}
void OnStaticDot() {}
void OnStaticQualified() {}
void OnOverload() {}

class Button {
public:
    void SetHandler(Callback cb) { cb(); }
    void operator()(Callback cb) { cb(); }
    // Implicit `this->SetHandler(cb)`.
    void Relay(Callback cb) { SetHandler(cb); }
    void RelayArrow(Callback cb) { this->SetHandler(cb); }
};

struct Slot {
    void operator()(Callback cb) { cb(); }
};

struct Panel {
    Slot slot;
};

class Fancy : public Button {
public:
    // A qualified call names the method directly.
    void Qualified(Callback cb) { Button::SetHandler(cb); }
};

class Picker {
public:
    void Pick(int n) { (void)n; }
    void Pick(Callback cb) { cb(); }
    void Pick(Callback cb, int n) { (void)n; cb(); }
};

class PickerUser : public Picker {
public:
    // Two explicit arguments: only the two-parameter overload takes them.
    void Use(Callback cb) { Picker::Pick(cb, 1); }
};

void Wire(Button &b, Button *p, Panel &panel, Remote &r) {
    b.SetHandler(OnDot);
    Callback f = OnVar;
    b.SetHandler(f);
    p->SetHandler(OnArrow);
    b(OnFunctor);
    panel.slot(OnFieldFunctor);
    r.Later(OnRemote);
    r.Shared(OnStaticDot);
    Remote::Shared(OnStaticQualified);
}

void Indirect(Button &b, Fancy &x, PickerUser &u) {
    b.Relay(OnImplicit);
    b.RelayArrow(OnThisArrow);
    x.Qualified(OnQualified);
    u.Use(OnOverload);
}
