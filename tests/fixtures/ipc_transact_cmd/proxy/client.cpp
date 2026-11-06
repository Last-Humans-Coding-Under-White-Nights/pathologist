#include "../interface.h"

namespace OHOS {
namespace AppExecFwk {
class IRemoteObject {
public:
    int SendRequest(int code);
};
IRemoteObject *Remote();
void LogSendTransactCmdResult();

class TransportHelper {
public:
    template <typename T> int SendTransactCmd(T code);
};

// Reduced from ability_runtime's RenderStateObserverProxy: the public
// callback calls a local transport helper, which forwards to SendRequest.
class RenderStateObserverProxy : public IRenderStateObserver {
public:
    void OnRenderStateChanged(int state) override;
    int Notify();
    int Changed();
    int DuplicateSends();
    int TemplateNotify();
    int HelperNotify();
    void LocalOnly();
    void AsyncOnly();
    void UnevaluatedOnly();

private:
    int SendTransactCmd(int code);
    template <typename T> int SendTransactCmd(T code);
    int SendTransactCmdAsync(int code);
    TransportHelper *helper_;
};

void RenderStateObserverProxy::OnRenderStateChanged(int state)
{
    SendTransactCmd(state);
}

int RenderStateObserverProxy::SendTransactCmd(int code)
{
    return Remote()->SendRequest(code);
}

int RenderStateObserverProxy::Notify()
{
    return this->SendTransactCmd(2);
}

int RenderStateObserverProxy::DuplicateSends()
{
    // Several sends in one method still produce one bridge per handler.
    Remote()->SendRequest(2);
    return this->SendTransactCmd(2);
}

int RenderStateObserverProxy::TemplateNotify()
{
    return this->template SendTransactCmd<int>(2);
}

int RenderStateObserverProxy::HelperNotify()
{
    return helper_->template SendTransactCmd<int>(2);
}

int RenderStateObserverProxy::Changed()
{
    return RenderStateObserverProxy::SendTransactCmd(3);
}

void RenderStateObserverProxy::LocalOnly()
{
    LogSendTransactCmdResult();
}

void RenderStateObserverProxy::AsyncOnly()
{
    SendTransactCmdAsync(4);
}

void RenderStateObserverProxy::UnevaluatedOnly()
{
    sizeof(SendTransactCmd(5));
}

// Bodyless helpers must work too, including the *Client naming convention.
class RenderStateObserverClient {
public:
    int Notify() { return SendTransactCmd(2); }
private:
    int SendTransactCmd(int code);
};
}
}
