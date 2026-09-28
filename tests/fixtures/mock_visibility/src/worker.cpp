#include "worker.h"
int ns::Worker::Run() { return 1; }
int ns::Worker::SubmitTask(int a) { return a; }
int ns::Worker::SubmitTask(int a,int b) { return b; }
int ns::Worker::SubmitTask(int a,int b,int c) { return c; }
