// Member calls on receivers whose class is never declared in the unit (#193).
// The classes live in another repository whose headers are not on the
// include path; the include below resolves to nothing.
#include "message_parcel.h"

namespace OHOS {
class Declared;

// Parameters: by reference (`.`) and by pointer (`->`), beside a
// forward-declared class for comparison.
int Send(MessageParcel &data, MessageParcel *reply, Declared &d) {
    data.WriteInt32(1);
    reply->ReadInt32();
    d.Run();
    return 0;
}

// Fields of an undeclared class.
struct Holder {
    MessageOption option;
    MessageOption *optionPtr;
    void Go() {
        option.SetFlags(1);
        optionPtr->GetFlags();
    }
};

// Locals of an undeclared class.
int Local() {
    Parcel parcel;
    Parcel *parcelPtr = &parcel;
    parcel.WriteString("x");
    parcelPtr->ReadString();
    return 0;
}

// The class is placed in the namespace the type is written in.
namespace AAFwk {
int Nested(Want &want) {
    want.SetParam(1);
    return 0;
}
}

// A template parameter is not a class name: stays unresolved.
template <typename T> int Generic(T t, T *tp) {
    t.Run();
    tp->Run();
    return 0;
}

// An undeclared wrapper of an unknown class stays unresolved.
int Wrapped(sptr<Unknown> p) {
    p->Run();
    return 0;
}

// A callback field whose typedef the unit never sees (`OnSwitch`, from a
// header off the include path) still holds the function stored into it:
// the call through it reaches that function, as it did when the typedef
// read as `int`.
void OnSwitchHandler(int);
struct Listener {
    OnSwitch handler_;
    Listener(OnSwitch handler) : handler_(handler) {}
    void Fire() { handler_(1); }
};
int UseListener() {
    Listener l(OnSwitchHandler);
    l.Fire();
    return 0;
}

// `T f();` in a body declares a function, as it does at namespace scope:
// its return type is not guessed.
int InBody() {
    Unreturned Get();
    return 0;
}

// #147: the callee is named from the receiver's type, never from the bare
// member name, so a free function of that name is not reached.
void handler(int);
int Field(Unseen *u) {
    u->handler(1);
    return 0;
}
}

// Global scope: no namespace prefix.
int Fuzz(FuzzedDataProvider &fdp) {
    fdp.ConsumeBool();
    return 0;
}
