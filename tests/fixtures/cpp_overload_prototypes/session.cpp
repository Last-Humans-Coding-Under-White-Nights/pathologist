#include "session.hpp"

Mode Session::Get() { return MODE_A; }
int Session::Get(Mode &mode) { mode = MODE_B; return 0; }
void Session::Run(Callback cb) { cb(); }
void Session::Run(Callback cb, int times) { (void)times; cb(); }
void Session::Log(Callback cb, int level) { (void)level; cb(); }
void Session::Log(const char *text) { (void)text; }
void Session::Emit(Callback cb, ...) { cb(); }
void Session::Emit(Callback cb) { (void)cb; }
