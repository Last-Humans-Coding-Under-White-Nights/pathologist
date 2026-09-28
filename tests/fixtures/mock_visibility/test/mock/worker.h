#pragma once
namespace ns { struct Worker { int Run() { return 42; } int SubmitTask(int a) { return a; } int SubmitTask(int a,int b) { return b; } int SubmitTask(int a,int b,int c) { return c; } }; }
