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
