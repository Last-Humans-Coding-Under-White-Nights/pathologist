// OpenHarmony task and thread primitives (#203). As in the corpora, the
// headers declaring them (ffrt, eventhandler, c_utils, ipc) are not in the
// tree: every API below is a class or function the unit never declares, so
// the models must match the names lowering gives them.
#include <future>
#include <memory>
#include <thread>

namespace OHOS {
namespace CameraStandard {

void FfrtTask() {}
void FfrtHandleTask() {}
void QueueHandleTask() {}
void Posted() {}
void Immediate() {}
void HighPriority() {}
void Idle() {}
void Sync() {}
void Timing() {}
void AtFront() {}
void Pooled() {}
void TimerTick() {}
void AsyncJob() {}
void DeferredJob() {}
void JThreadJob() {}
void QueuedOnParam() {}
void PostedOnParam() {}
void Kicked() {}
void OnSubclass() {}

class Session {
public:
    void Submit();
    void Tick() {}

private:
    std::shared_ptr<AppExecFwk::EventHandler> handler_;
    std::shared_ptr<ffrt::queue> queue_;
};

void Session::Submit()
{
    ffrt::submit(FfrtTask);
    ffrt::submit_h(FfrtHandleTask, {}, {});
    ffrt::submit([this] { Tick(); }, {}, {}, ffrt::task_attr().name("tick"));
    queue_->submit_h(QueueHandleTask);

    // The `PostTask` family, on a handler held in a field.
    handler_->PostTask(Posted, "posted", 0);
    handler_->PostImmediateTask(Immediate);
    handler_->PostHighPriorityTask(HighPriority);
    handler_->PostIdleTask(Idle);
    handler_->PostSyncTask(Sync);
    handler_->PostTimingTask(Timing, 100);
    handler_->PostTaskAtFront(AtFront);

    // `ThreadPool` written inside `OHOS::CameraStandard` may be
    // `OHOS::ThreadPool`, which it is.
    ThreadPool pool;
    pool.AddTask(Pooled);
    Utils::Timer timer("camera");
    timer.Register(TimerTick, 100);

    auto launched = std::async(std::launch::async, AsyncJob);
    auto deferred = std::async(DeferredJob);
    std::jthread worker(JThreadJob);
}

// The receiver is a named variable: the queue or handler is recorded.
void Enqueue(ffrt::queue &queue, AppExecFwk::EventHandler *handler)
{
    queue.submit_h(QueuedOnParam);
    handler->PostTask(PostedOnParam);
}

// Virtual members a framework runs: every override is an entry.
class Worker : public Thread {
    bool Run() override { return false; }
};

class Handler : public AppExecFwk::EventHandler {
public:
    void ProcessEvent(const AppExecFwk::InnerEvent::Pointer &event) override {}
    void Kick() { this->PostTask(Kicked); }
};

// A handler subclass posts through the member it inherits.
void PostOnSubclass(Handler *handler) { handler->PostTask(OnSubclass); }

class Recipient : public IRemoteObject::DeathRecipient {
public:
    void OnRemoteDied(const wptr<IRemoteObject> &remote) override;
};
void Recipient::OnRemoteDied(const wptr<IRemoteObject> &remote) {}

} // namespace CameraStandard

// Classes the tree declares are what they are named, whatever their bases
// and members are called: hiview's own `EventHandler` is not the
// eventhandler library's, and a `Thread` of its own is not c_utils'.
namespace HiviewDFX {
void NotATask() {}
void NotMail() {}
class EventHandler {
public:
    virtual void ProcessEvent(int event);
    bool PostTask(void (*task)());
};
class Plugin : public EventHandler {
    void ProcessEvent(int event) override {}
};
class Thread {
public:
    virtual bool Run();
};
class Loop : public Thread {
    bool Run() override { return true; }
};
class Mailbox {
public:
    void AddTask(void (*task)());
};
void UsePlugin(Plugin &plugin, Mailbox &mailbox)
{
    plugin.PostTask(NotATask);
    mailbox.AddTask(NotMail);
}
} // namespace HiviewDFX
} // namespace OHOS
