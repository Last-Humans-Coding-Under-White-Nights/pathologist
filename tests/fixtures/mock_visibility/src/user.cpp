#include "worker.h"
namespace ns { int use(Worker *w) { return w->Run(); } int submit(Worker *w) { return w->SubmitTask(1); } }
