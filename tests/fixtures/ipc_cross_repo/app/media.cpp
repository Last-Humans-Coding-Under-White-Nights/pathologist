namespace OHOS {
namespace AbilityRuntime {
class MediaDataShareExtAbility {
public:
    int OpenFile(int uri) { return uri; }
};
}
namespace DataShare {
using DataShare::MediaDataShareExtAbility;
class MediaDataShareStubImpl : public DataShareStub {
    std::shared_ptr<MediaDataShareExtAbility> extension_;
    std::shared_ptr<MediaDataShareExtAbility> GetOwner() { return extension_; }
public:
    int OpenFile(int uri) {
        auto client = sptr<MediaDataShareStubImpl>(this);
        auto extension = client->GetOwner();
        return extension->OpenFile(uri);
    }
};
}
namespace Media {
int OpenAsset(DataShare::DataShareHelper *helper) { return helper->OpenFile(1); }
}
}
