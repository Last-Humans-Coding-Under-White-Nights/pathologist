// A variadic overload beside a fixed one of the arity the call passes.
void Log(void *first, ...) { (void)first; }
void Log(int first, int second) { (void)first; (void)second; }

void LogPointers(void *p) { Log(p, p); }

// Both fit by arity, and the fixed overload can take the arguments: C++
// ranks the variadic one below it.
void Notify(const char *text, int code) { (void)text; (void)code; }
void Notify(const char *format, ...) { (void)format; }

void NotifyOnce() { Notify("ready", 1); }

// An argument of unknown type cannot rule the variadic overload out.
void Trace(int level) { (void)level; }
void Trace(const char *format, ...) { (void)format; }

void TraceUnknown() { Trace(NotDeclaredAnywhere()); }

// A floating-point argument cannot bind a pointer parameter.
void Show(const char *text) { (void)text; }
void Show(double value, ...) { (void)value; }

void ShowNumber() { Show(2.5); }
