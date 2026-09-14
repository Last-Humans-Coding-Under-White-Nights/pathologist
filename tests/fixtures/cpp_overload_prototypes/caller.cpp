#include "session.hpp"
#include "listener.hpp"
#include "walker.hpp"

void OnRun() {}
void OnFire() {}
void OnLog() {}

int GetWithOut(Session *s) {
    Mode mode;
    return s->Get(mode);
}

Mode GetPlain(Session *s) { return s->Get(); }

void RunTwice(Session *s) { s->Run(OnRun, 2); }

void LogWithDefault(Session *s) { s->Log(OnLog); }

void OnEmit() {}

void EmitMore(Session *s) { s->Emit(OnEmit, 1, 2); }

void FireNamed(Listener &l) { l.Fire("ready", OnFire); }

bool WalkAll(Walker &w) { return w.Walk(1); }
