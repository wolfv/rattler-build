#include <stdio.h>
#include "shapes.h"

int main(void) {
  printf("shapes %s: circle(1)=%.3f square(2)=%.1f triangle(3,4)=%.1f hexagon(1)=%.3f\n",
         shapes_version(), circle_area(1), square_area(2), triangle_area(3, 4), hexagon_area(1));
  return 0;
}
