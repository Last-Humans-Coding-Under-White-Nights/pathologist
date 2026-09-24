class FlowPayload {
public:
    void (*read_cb)();
    void (*write_cb)();
    void (*nested_cb)();
};
template<class T> class FlowWrapper {
public:
    T *operator->();
    T &operator*();
    void (*read_cb)(); // Same spelling as the pointee, different storage.
};
class FlowHolder { public: missing<FlowPayload> item; };

void FlowReadTarget() {}
void FlowWriteTarget() {}
void FlowNestedTarget() {}
void FlowWrapperOwnTarget() {}
void FlowSetRaw(FlowPayload *p) { p->read_cb = FlowReadTarget; }
void FlowReadMissing(missing<FlowPayload> p) { p->read_cb(); }
void FlowReadDeclared(FlowWrapper<FlowPayload> p) { p->read_cb(); }
void FlowReadDerefDot(missing<FlowPayload> p) { (*p).read_cb(); }
void FlowReadDerefDotDeclared(FlowWrapper<FlowPayload> p) { (*p).read_cb(); }
void FlowReadDerefDotRaw(FlowPayload *p) { (*p).read_cb(); }
void FlowReadDerefDotWrapperRaw(FlowWrapper<FlowPayload> *p) { (*p).read_cb(); }
void FlowReadReference(missing<FlowPayload> &p) { p->read_cb(); }
void FlowReadDereference(missing<FlowPayload> *p) { (*p)->read_cb(); }
void FlowReadStandard(std::shared_ptr<FlowPayload> p) { p->read_cb(); }
void FlowSetMissing(missing<FlowPayload> p) { p->write_cb = FlowWriteTarget; }
void FlowReadRaw(FlowPayload *p) { p->write_cb(); }
void FlowDerefWriteTarget() {}
void FlowSetDerefDot(missing<FlowPayload> p) { (*p).write_cb = FlowDerefWriteTarget; }
void FlowSetNested(FlowHolder h) { h.item->nested_cb = FlowNestedTarget; }
void FlowReadNested(FlowHolder h) { h.item->nested_cb(); }
void FlowReadTwoArrows(missing<FlowHolder> h) { h->item->nested_cb(); }
void FlowReadNestedRaw(FlowPayload *p) { p->nested_cb(); }
void FlowSetWrapperOwn(FlowWrapper<FlowPayload> *p) { p->read_cb = FlowWrapperOwnTarget; }
void FlowReadWrapperOwn(FlowWrapper<FlowPayload> *p) { p->read_cb(); }
class FlowArrowBase {
public:
    FlowPayload *operator->();
};
class FlowArrowDerived : public FlowArrowBase {};
void FlowReadInherited(FlowArrowDerived d) { d->read_cb(); }

// Parentheses inside a member chain do not end the path (macro expansions
// and defensive code write them).
class FlowParenMid { public: FlowPayload *child; };
void FlowSetParen(FlowParenMid *m) { m->child->read_cb = FlowReadTarget; }
void FlowReadParen(FlowParenMid *m) { (m->child)->read_cb(); }
void FlowReadParenWrapper(missing<FlowParenMid> m) { (m->child)->read_cb(); }

// Wrapper storage stays out of the pointee once wrapper values carry
// locations (#141): an address-taken wrapper whose own member has the same
// name and position as its pointee's.
class IsoPayload { public: void (*cb)(); };
template<class T> class IsoWrapper {
public:
    void (*cb)();
    T *operator->();
    T &operator*();
};
void IsoPointeeTarget() {}
void IsoWrapperTarget() {}
IsoWrapper<IsoPayload> iso_wrapper;
IsoWrapper<IsoPayload> *iso_wrapper_addr = &iso_wrapper;
void IsoSetPointee(IsoPayload *p) { p->cb = IsoPointeeTarget; }
void IsoSetWrapperDot() { iso_wrapper.cb = IsoWrapperTarget; }
void IsoSetWrapperRaw(IsoWrapper<IsoPayload> *w) { w->cb = IsoWrapperTarget; }
void IsoReadArrow() { iso_wrapper->cb(); }
void IsoReadDeref() { (*iso_wrapper).cb(); }
void IsoReadReference(IsoWrapper<IsoPayload> &w) { w->cb(); }
void IsoCallReference() { IsoReadReference(iso_wrapper); }
void IsoReadPointerArrow(IsoWrapper<IsoPayload> *w) { (*w)->cb(); }
void IsoCallPointerArrow() { IsoReadPointerArrow(&iso_wrapper); }
void IsoReadWrapperDot() { iso_wrapper.cb(); }
void IsoReadWrapperRaw() { iso_wrapper_addr->cb(); }
