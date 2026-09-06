#include <stdio.h>
int main(void){unsigned long f; __asm__ volatile("mrs %0, cntfrq_el0":"=r"(f));printf("%lu\n",f);}
