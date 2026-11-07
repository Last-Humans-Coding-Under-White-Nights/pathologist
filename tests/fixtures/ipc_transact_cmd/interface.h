#pragma once

namespace OHOS {
namespace AppExecFwk {
class IRenderStateObserver {
public:
    virtual void OnRenderStateChanged(int state) = 0;
};
}
}
