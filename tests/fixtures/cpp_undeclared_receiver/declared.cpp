// The same calls with the classes forward-declared: the undeclared unit must
// produce exactly these edges.
namespace OHOS {
class MessageParcel;
class MessageOption;
class Parcel;
namespace AAFwk { class Want; }
class Unseen;

int SendDeclared(MessageParcel &data, MessageParcel *reply) {
    data.WriteInt32(1);
    reply->ReadInt32();
    return 0;
}

struct HolderDeclared {
    MessageOption option;
    MessageOption *optionPtr;
    void Go() {
        option.SetFlags(1);
        optionPtr->GetFlags();
    }
};

int LocalDeclared() {
    Parcel parcel;
    Parcel *parcelPtr = &parcel;
    parcel.WriteString("x");
    parcelPtr->ReadString();
    return 0;
}

namespace AAFwk {
int NestedDeclared(Want &want) {
    want.SetParam(1);
    return 0;
}
}

void handler(int);
int FieldDeclared(Unseen *u) {
    u->handler(1);
    return 0;
}
}

class FuzzedDataProvider;
int FuzzDeclared(FuzzedDataProvider &fdp) {
    fdp.ConsumeBool();
    return 0;
}
