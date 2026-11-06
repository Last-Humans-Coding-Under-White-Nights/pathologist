#include "../interface.h"

namespace OHOS {
namespace AppExecFwk {
class RenderStateObserverStub : public IRenderStateObserver {
public:
    int OnRemoteRequest(int code) { return code; }
    void OnRenderStateChangedInner(int state) { OnRenderStateChanged(state); }
    int HandleNotify() { return 2; }
    int ChangedStub() { return 3; }
    int HandleDuplicateSends() { return 2; }
    int HandleTemplateNotify() { return 2; }
    int HandleHelperNotify() { return 2; }
    void LocalOnly() {}
    void AsyncOnly() {}
    void UnevaluatedOnly() {}
};

// Like the real observer stub, dispatch calls an inherited interface method.
// The bridge should reach the concrete implementation when it is indexed.
class RenderStateObserverService : public RenderStateObserverStub {
public:
    void OnRenderStateChanged(int state) override {}
};
}
}
