// Self-contained Linux ABI declarations: do not depend on host headers.
union epoll_data { void *ptr; int fd; unsigned long long u64; };
struct epoll_event { unsigned int events; epoll_data data; };
extern "C" int epoll_ctl(int, int, int, epoll_event *);
extern "C" int epoll_wait(int, epoll_event *, int, int);
extern "C" int epoll_pwait(int, epoll_event *, int, int, const void *);
extern "C" int epoll_pwait2(int, epoll_event *, int, const void *, const void *);

struct LevelHandler { void (*handler)(int, unsigned int); int data; };
void HandleLevelReport(int level, unsigned int events) {}
LevelHandler levelHandler = { HandleLevelReport, 0 };

void register_event(int epfd, int fd, void *data) {
    epoll_event registration;
    registration.events = 2;
    registration.data.ptr = data;
    epoll_ctl(epfd, 1, fd, &registration);
}
void monitor(int epfd, int fd) { register_event(epfd, fd, &levelHandler); }

void HandleEpollEvent(epoll_event *event) {
    LevelHandler *handler = (LevelHandler *)event->data.ptr;
    handler->handler(handler->data, event->events);
}
void wait_events(int epfd) {
    epoll_event events[8];
    int count = epoll_wait(epfd, events, 8, -1);
    for (int i = 0; i < count; ++i) HandleEpollEvent(&events[i]);
}
void pwait_events(int epfd) {
    epoll_event events[8];
    epoll_pwait(epfd, events, 8, -1, 0);
    LevelHandler *handler = (LevelHandler *)events[0].data.ptr;
    handler->handler(handler->data, events[0].events);
}
void pwait2_events(int epfd) {
    epoll_event events[8];
    epoll_pwait2(epfd, events, 8, 0, 0);
    LevelHandler *handler = (LevelHandler *)events[0].data.ptr;
    handler->handler(handler->data, events[0].events);
}
// A wait without a dispatch must not itself synthesize a callback edge.
void only_wait(int epfd) {
    epoll_event events[8];
    epoll_wait(epfd, events, 8, -1);
}
// Qualified lookalikes must not take the libc model.
namespace other {
int epoll_wait(int, epoll_event *, int, int);
void wait_events(int epfd) {
    epoll_event events[8];
    epoll_wait(epfd, events, 8, -1);
    LevelHandler *handler = (LevelHandler *)events[0].data.ptr;
    handler->handler(handler->data, events[0].events);
}
}

// OHOS's nested configuration table and member-owned dispatch pointer.
void NestedReport(int level, unsigned int events) {}
struct Config { int level; LevelHandler levelHandler; };
static Config configs[1] = {{0}};
class Observer {
    LevelHandler *handlerInfo = nullptr;
public:
    void monitor(int epfd, int fd, int level) {
        configs[level].levelHandler.handler = NestedReport;
        configs[level].levelHandler.data = level;
        register_event(epfd, fd, &configs[level].levelHandler);
    }
    void handle(epoll_event *event) {
        handlerInfo = (LevelHandler *)event->data.ptr;
        handlerInfo->handler(handlerInfo->data, event->events);
    }
    void loop(int epfd) {
        epoll_event events[8];
        int n = epoll_wait(epfd, events, 8, -1);
        for (int i = 0; i < n; ++i) handle(&events[i]);
    }
};

// Resolve the wait API indirectly, before the registration API is discovered.
void indirect_wait(int epfd) {
    int (*wait_api)(int, epoll_event *, int, int) = epoll_wait;
    epoll_event late_events[8];
    wait_api(epfd, late_events, 8, -1);
    HandleEpollEvent(&late_events[0]);
}
void indirect_register(int epfd, int fd) {
    int (*ctl_api)(int, int, int, epoll_event *) = epoll_ctl;
    epoll_event late_registration;
    late_registration.data.ptr = &levelHandler;
    ctl_api(epfd, 3, fd, &late_registration); // EPOLL_CTL_MOD
    epoll_ctl(epfd, 2, fd, nullptr); // EPOLL_CTL_DEL: no new payload
}

void array_register(int epfd, int fd) {
    epoll_event array_registration[1];
    array_registration[0].data.ptr = &levelHandler;
    epoll_ctl(epfd, 1, fd, array_registration);
}

void mutable_register(int epfd, int fd) {
    epoll_event mutable_registration;
    mutable_registration.data.ptr = &levelHandler;
    epoll_event *event = nullptr;
    event = &mutable_registration;
    epoll_ctl(epfd, 1, fd, event);
}
