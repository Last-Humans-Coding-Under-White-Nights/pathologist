class RawPayload { public: int payload_value; };
template<class T> class RawWrapper {
public:
    T *operator->();
    void (*own_callback)();
    void (*nested_callback)();
};
class RawWrapperHolder { public: RawWrapper<RawPayload> *wrapper; };

void RawWrapperTarget() {}
void RawWrapperDirect(RawWrapper<RawPayload> *p) {
    p->own_callback = RawWrapperTarget;
    p->own_callback();
}
void RawWrapperNested(RawWrapperHolder h) {
    h.wrapper->nested_callback = RawWrapperTarget;
    h.wrapper->nested_callback();
}
void RawWrapperReference(RawWrapper<RawPayload> &p) {
    int value = p->payload_value;
}
void RawWrapperDereference(RawWrapper<RawPayload> *p) {
    int value = (*p)->payload_value;
}
