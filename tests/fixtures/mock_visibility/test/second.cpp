namespace ns { struct Worker { int Run() { return 43; } int SubmitTask(int a) { return a; } }; int second_use(Worker *w) { return w->Run(); } }
