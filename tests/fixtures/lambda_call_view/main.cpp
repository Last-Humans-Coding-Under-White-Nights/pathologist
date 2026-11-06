// A small version of UIAbilityThread's scheduling pattern, without SDK headers.
namespace OHOS {
namespace AppExecFwk {
class EventHandler;
}
class UIAbilityThread {
public:
    void ScheduleAbilityTransaction();
    void HandleAbilityTransaction() {}
private:
    AppExecFwk::EventHandler *handler_;
};

void UIAbilityThread::ScheduleAbilityTransaction()
{
    auto task = [this] {
        HandleAbilityTransaction();
    };
    handler_->PostTask(task);
}
}
