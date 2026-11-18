/*
 * The end of file the host console sends when its input ends (tuxport-run --pty with stdin a pipe,
 * as util-linux's script does) must still be an end of file for a program that leaves canonical
 * mode before reading it, as a shell does when it returns to its line editor: the program reads
 * the VEOF character there, not the NUL Linux leaves for a VEOF typed in canonical mode. Reads the
 * line sent in, waits in canonical mode while the console sends its end of file, leaves canonical
 * mode and prints the byte it then reads in hex.
 */
#include <stdio.h>
#include <termios.h>
#include <unistd.h>

int main(void) {
    char line[64];
    if (read(0, line, sizeof line) <= 0)
        return 1;
    /* The console sends its end of file once the line has been read */
    sleep(1);
    struct termios t;
    if (tcgetattr(0, &t) != 0)
        return 2;
    t.c_lflag &= ~(ICANON | ECHO);
    t.c_cc[VMIN] = 1;
    t.c_cc[VTIME] = 0;
    if (tcsetattr(0, TCSANOW, &t) != 0)
        return 3;
    unsigned char c;
    if (read(0, &c, 1) != 1)
        return 4;
    printf("byte %02x\n", c);
    return 0;
}
