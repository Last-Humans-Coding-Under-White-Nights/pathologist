/* HDF work queues and timers, and ffrt's C submission forms (#203). The
 * HDF OSAL headers are not in the tree here; the corpus defines them, which
 * the models do not depend on. */
#include <stdint.h>

typedef void (*HdfWorkFunc)(void *);
typedef void (*OsalTimerFunc)(uintptr_t);
typedef struct { void *realWork; } HdfWork;
typedef struct { void *realTimer; } OsalTimer;
typedef struct ffrt_function_header ffrt_function_header_t;

int HdfWorkInit(HdfWork *work, HdfWorkFunc func, void *arg);
int HdfDelayedWorkInit(HdfWork *work, HdfWorkFunc func, void *arg);
int OsalTimerCreate(OsalTimer *timer, uint32_t interval, OsalTimerFunc func, uintptr_t arg);

struct Ctx {
    void (*cb)(void);
};
void OnWork(void) {}

static void WorkFn(void *arg) { ((struct Ctx *)arg)->cb(); }
static void DelayedFn(void *arg) {}
static void TimerFn(uintptr_t arg) {}

void SetupWork(HdfWork *work, HdfWork *delayed, OsalTimer *timer, struct Ctx *ctx)
{
    ctx->cb = OnWork;
    HdfWorkInit(work, WorkFn, ctx);
    HdfDelayedWorkInit(delayed, DelayedFn, ctx);
    OsalTimerCreate(timer, 100, TimerFn, (uintptr_t)ctx);
}

/* The C forms take a function header whose `exec` the runtime calls; the
 * models invoke what the header argument itself may hold. */
static void CTask(void) {}
static void CHandleTask(void) {}
static void CQueueTask(void) {}
static void CQueueHandleTask(void) {}
void SubmitC(void *queue)
{
    ffrt_submit_base((ffrt_function_header_t *)CTask, 0, 0, 0);
    ffrt_submit_h_base((ffrt_function_header_t *)CHandleTask, 0, 0, 0);
    ffrt_queue_submit(queue, (ffrt_function_header_t *)CQueueTask, 0);
    ffrt_queue_submit_h(queue, (ffrt_function_header_t *)CQueueHandleTask, 0);
}
