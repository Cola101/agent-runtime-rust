package com.agentplatform.control.security;

import org.springframework.beans.factory.annotation.Value;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.core.annotation.Order;
import org.springframework.http.HttpMethod;
import org.springframework.security.config.Customizer;
import org.springframework.security.config.annotation.web.builders.HttpSecurity;
import org.springframework.security.core.userdetails.User;
import org.springframework.security.core.userdetails.UserDetailsService;
import org.springframework.security.provisioning.InMemoryUserDetailsManager;
import org.springframework.security.web.SecurityFilterChain;

@Configuration
public class SecurityConfiguration {
  @Bean
  UserDetailsService managementScrapeUser(
      @Value("${spring.security.user.name}") String username,
      @Value("${spring.security.user.password}") String password) {
    return new InMemoryUserDetailsManager(User.withUsername(username)
        .password("{noop}" + password)
        .roles("METRICS")
        .build());
  }

  @Bean
  @Order(1)
  SecurityFilterChain managementSecurity(HttpSecurity http) throws Exception {
    return http
        .securityMatcher("/actuator/**")
        .csrf(csrf -> csrf.disable())
        .authorizeHttpRequests(authorize -> authorize
            .requestMatchers("/actuator/health/**").permitAll()
            .anyRequest().hasRole("METRICS"))
        .httpBasic(Customizer.withDefaults())
        .build();
  }

  @Bean
  @Order(2)
  SecurityFilterChain apiSecurity(HttpSecurity http) throws Exception {
    return http
        .csrf(csrf -> csrf.disable())
        .authorizeHttpRequests(authorize -> authorize
            .requestMatchers(HttpMethod.GET, "/v1/console/resource-context")
            .hasAuthority("SCOPE_resources:read")
            .requestMatchers(HttpMethod.POST, "/v1/workspaces", "/v1/agents",
                "/v1/agents/*/versions", "/v1/model-providers", "/v1/model-policies",
                "/v1/sessions")
            .hasAuthority("SCOPE_resources:write")
            .requestMatchers(HttpMethod.POST, "/v1/sessions/*/runs")
            .hasAuthority("SCOPE_runs:write")
            .requestMatchers(HttpMethod.POST, "/v1/runs/*:cancel", "/v1/runs/*:steer")
            .hasAuthority("SCOPE_runs:write")
            .requestMatchers(HttpMethod.GET, "/v1/console/run-targets")
            .hasAuthority("SCOPE_runs:write")
            .requestMatchers(HttpMethod.POST, "/v1/approvals/*:decide")
            .hasAuthority("SCOPE_approvals:write")
            .requestMatchers(HttpMethod.GET, "/v1/approvals")
            .hasAuthority("SCOPE_approvals:read")
            .requestMatchers(HttpMethod.GET, "/v1/runs")
            .hasAuthority("SCOPE_runs:read")
            .requestMatchers(HttpMethod.GET, "/v1/runs/*/events")
            .hasAuthority("SCOPE_runs:read")
            .anyRequest().authenticated())
        .oauth2ResourceServer(resourceServer -> resourceServer.jwt(Customizer.withDefaults()))
        .build();
  }
}
