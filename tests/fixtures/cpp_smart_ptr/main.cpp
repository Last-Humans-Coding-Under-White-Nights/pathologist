// Member access through a declared `operator->` (#64).
//
// The wrapper is recognised by what it declares, not by what it is called:
// the three cases below differ only in the wrapper's name.

class CaptureSession {
public:
    virtual int AddOutput(int x);
};
int CaptureSession::AddOutput(int x) { return x + 1; }

// 1. A class template whose `operator->` returns its own parameter is a
//    smart pointer, whatever it is named.
template <typename T>
class sptr {
public:
    T *operator->() const { return ptr_; }
    T *GetRefPtr() const { return ptr_; }

private:
    T *ptr_;
};

template <typename T>
class RefPtr {
public:
    T *operator->() const { return ptr_; }

private:
    T *ptr_;
};

int UseSptr(sptr<CaptureSession> session) { return session->AddOutput(1); }
int UseRefPtr(RefPtr<CaptureSession> session) { return session->AddOutput(2); }
int UseDot(sptr<CaptureSession> session) { return session.GetRefPtr() != 0; }

// 2. A wrapper whose class is absent from the tree: `shared_ptr` is never
//    declared here, so only its name is left to go on.
int UseShared(shared_ptr<CaptureSession> session) { return session->AddOutput(3); }

// 3. A chain of non-template wrappers, each `operator->` naming the next.
class Inner {
public:
    CaptureSession *operator->() const;
};
class Outer {
public:
    Inner operator->() const;
};
int UseChain(Outer outer) { return outer->AddOutput(4); }

// 4. An `operator->` cycle names no pointee: the site stays unresolved
//    rather than gaining an invented member.
class Pong;
class Ping {
public:
    Pong operator->() const;
};
class Pong {
public:
    Ping operator->() const;
};
int UseCycle(Ping ping) { return ping->AddOutput(5); }

// 5. A wrapper reached through a field, and a wrapper reached through a
//    wrapper's own member type.
class Holder {
public:
    sptr<CaptureSession> session_;
};
int UseField(Holder *holder) { return holder->session_->AddOutput(6); }

class PointerTarget {
public:
    CaptureSession *operator->() const;
    int Own() { return 1; }
    int ExplicitThis() { return this->Own(); }
};
class PointerOuter {
public:
    PointerTarget *operator->() const;
};
int UseRaw(PointerTarget *p) { return p->Own(); }
int UsePointerReturn(PointerOuter p) { return p->Own(); }
int UseReference(PointerTarget &p) { return p->AddOutput(1); }
template<class Metadata, class T>
class SecondArg {
public:
    T *operator->() const;
};
int UseSecond(SecondArg<PointerTarget, CaptureSession> p) { return p->AddOutput(1); }

class RawHolder { public: PointerTarget *target; };
int UseRawField(RawHolder h) { return h.target->Own(); }
int UseLocalReference(PointerTarget &p) { PointerTarget &r = p; return r->AddOutput(1); }
class OtherTarget { public: int AddOutput(int x) { return x; } };
template<class T> class Ambiguous {
public:
    CaptureSession *operator->();
    OtherTarget *operator->() const;
};
int UseAmbiguous(Ambiguous<PointerTarget> p) { return p->AddOutput(1); }

template<class T> class ArrowBase { public: T *operator->(); };
template<class Ignored> class ArrowDerived : public ArrowBase<CaptureSession> {};
int UseInheritedTemplate(ArrowDerived<PointerTarget> p) { return p->AddOutput(1); }
template<class T> class NestedDependent { public: sptr<T> operator->(); };
int UseNestedDependent(NestedDependent<CaptureSession> p) { return p->AddOutput(1); }

void CallbackTarget() {}
template<class T> class CallbackBox { public: void (*cb)(); };
void UseTemplateCallback() { CallbackBox<int> b; b.cb = CallbackTarget; b.cb(); }
template<class T> class CallbackHandle {
public:
    CallbackHandle(int n) {}
    T *operator->();
    void (*cb)();
};
void UseWrapperCallback() { CallbackHandle<CaptureSession> h(1); h.cb = CallbackTarget; h.cb(); }
class ConstructHolder {
public:
    CallbackHandle<CaptureSession> handle;
    ConstructHolder() : handle(1) {}
};

// 6. Dereferencing a wrapper yields its pointee, as `->` does — a smart
//    pointer's `operator*` and `operator->` agree — so a `.` after `(*w)`
//    looks the member up there. A raw pointer to a wrapper is the wrapper.
int UseDerefDot(sptr<CaptureSession> session) { return (*session).AddOutput(7); }
int UseDerefDotShared(shared_ptr<CaptureSession> session) { return (*session).AddOutput(8); }
int UseDerefArrow(sptr<CaptureSession> *pp) { return (*pp)->AddOutput(9); }

// 7. A `.` call stays on the wrapper even when the pointee declares a member
//    of the same name: `p.reset()` is the wrapper's, not `Resettable::reset`.
class Resettable {
public:
    int reset() { return 0; }
};
int UseDotShadow(shared_ptr<Resettable> p) { p.reset(); return 0; }

// 8. A class value with an `operator*` of its own but no `operator->` — an
//    iterator — dereferences to something this index does not follow: the
//    site stays unresolved rather than gaining an invented `Iter::AddOutput`.
class Iter {
public:
    CaptureSession *operator*() const;
};
int UseIterDeref(Iter it) { return (*it)->AddOutput(10); }
