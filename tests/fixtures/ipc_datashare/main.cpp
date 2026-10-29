// The DataShare and smart-pointer headers are outside the analyzed tree.
#ifdef DECLARED_SPTR
namespace OHOS {
template<class T> class sptr {
    T *pointer_;
public:
    sptr(T *p) : pointer_(p) {}
    T *operator->() const { return pointer_; }
};
}
#endif
namespace OHOS {
namespace AbilityRuntime {
class MediaDataShareExtAbility {
public:
    int OpenFile(int uri, int mode) { return uri + mode; }
};
}
namespace DataShare {
// In the real checkout this name depends on imports from a missing
// framework header; its definition is indexed in AbilityRuntime.
using DataShare::MediaDataShareExtAbility;

class MediaDataShareStubImpl : public DataShareStub {
    std::shared_ptr<MediaDataShareExtAbility> extension_;
    std::shared_ptr<MediaDataShareExtAbility> GetOwner() { return extension_; }
public:
    int OpenFile(int uri, int mode) {
        auto client = sptr<MediaDataShareStubImpl>(this);
        auto extension = client->GetOwner();
        return extension->OpenFile(uri, mode);
    }
};

// A second provider is a possible target too, including all overloads.
class OtherProvider : public DataShareStub {
public:
    int OpenFile(int uri, int mode) { return uri; }
    int OpenFile(int uri) { return uri; }
};
class LocalHelper : public DataShareHelper {
public:
    int OpenFile(int uri, int mode) { return uri; }
};
}
namespace Media {
class MediaLibraryManager {
    std::shared_ptr<DataShare::DataShareHelper> GetDataShareHelper();
public:
    int OpenAsset(int uri, int mode) {
        auto helper = GetDataShareHelper();
        return helper->OpenFile(uri, mode);
    }
};
}
}

// Same member spelling without the framework ancestry must not match.
namespace unrelated {
class DataShareHelper {
public:
    int OpenFile(int uri, int mode);
};
class DataShareStub {
public:
    int OpenFile(int uri, int mode) { return uri; }
};
int LocalOpen(DataShareHelper *helper) { return helper->OpenFile(1, 2); }
}

// Missing imports cannot select between namesakes or overloads.
namespace OHOS { namespace alternate {
class MediaDataShareExtAbility {
public:
    int OpenFile(int uri, int mode) { return uri; }
    int OpenFile(int uri) { return uri; }
};
}}
namespace declared {
class MediaDataShareExtAbility {};
// A known class's missing member stays external.
int MissingMember(MediaDataShareExtAbility *ability) { return ability->OpenFile(1, 2); }
}

namespace shadow {
class Receiver {};
class Result {
public:
    int Run() { return 1; }
};
template<class T> Result sptr(T *pointer);
int DeclaredFactory(Receiver *raw) {
    auto result = sptr<Receiver>(raw);
    return result.Run();
}
}
