import { createFileRoute } from "@tanstack/react-router";
import { TopNav } from "@/components/TopNav";
import { FullPageMain } from "@/components/FullPageMain";
import { MarketingHeader } from "@/components/MarketingHeader";
import { isAndroid } from "@/utils/platform";

export const Route = createFileRoute("/privacy")({
  component: Privacy
});

function Privacy(): JSX.Element {
  return (
    <>
      <TopNav />
      <FullPageMain>
        <MarketingHeader
          title={
            <h2 className="text-6xl font-light mb-0">
              <span className="dark:text-[hsl(var(--blue))] text-[hsl(var(--purple))]">
                Privacy
              </span>{" "}
              Notice
            </h2>
          }
          subtitle={
            <p className="text-2xl text-[hsl(var(--marketing-text-muted))] max-w-2xl mx-auto">
              Last revised on April 7, 2026
            </p>
          }
        />

        <div className="flex flex-col gap-8 text-foreground pt-8 max-w-4xl mx-auto w-full">
          <article
            className={
              "flex flex-col gap-4 dark:border-white/10 " +
              "border-[hsl(var(--marketing-card-border))] dark:bg-black/75 " +
              "bg-[hsl(var(--marketing-card))]/80 dark:text-white p-8 border rounded-lg " +
              "[&_h2]:text-2xl [&_h2]:font-medium [&_h2]:mt-6 [&_h2]:mb-2 " +
              "[&_p]:leading-relaxed " +
              "[&_ul]:list-disc [&_ul]:pl-6 [&_ul]:space-y-2 [&_li]:leading-relaxed " +
              "[&_a]:text-[hsl(var(--purple))] dark:[&_a]:text-[hsl(var(--blue))] " +
              "[&_a]:underline hover:[&_a]:no-underline " +
              "[&_strong]:font-semibold"
            }
          >
            <p>
              This privacy notice for Maple Privacy Labs Inc. ("Company," "we," "us," or "our"),
              describes how and why we might collect, store, use, and/or share ("process") your
              information when you use our services ("Services"). By using our Services, or by
              accessing our website{" "}
              {isAndroid() ? (
                "https://trymaple.ai"
              ) : (
                <a href="https://trymaple.ai">https://trymaple.ai</a>
              )}
              , <a href="https://opensecret.cloud">https://opensecret.cloud</a>, or the products
              Maple AI and OpenSecret, or any website of ours that links to this privacy notice, you
              are accepting and consenting to this Privacy Policy.
            </p>

            <p>
              Our mission is to provide you with secure, private, and encrypted services that don't
              share your personal data with third parties, but there is some information that we are
              obligated to take in and process on your behalf. Please read this notice carefully.
            </p>

            <h2>1. What information do we collect?</h2>
            <p>
              We take steps to minimize the information that we need to collect from you, but we do
              collect personal information that you voluntarily provide to us when you register on
              the Services, express an interest in obtaining information about us or our products
              and Services, when you participate in activities on the Services, or otherwise when
              you contact us.
            </p>
            <p>
              The personal information that we collect depends on the context of your interactions
              with us and the Services, the choices you make, and the products and features you use.
              The personal information we collect may include the following:
            </p>
            <ul>
              <li>Your Name</li>
              <li>Your Email</li>
              <li>Your Phone Number</li>
              <li>Social Media User Names for any accounts you link to the Services.</li>
              <li>Financial Information provided to us for payment purposes.</li>
              <li>
                Information from social media accounts that you use to log in to the Services.
              </li>
            </ul>
            <p>
              We automatically collect certain information when you visit, use, or navigate the
              Services. This information does not reveal your specific identity (like your name or
              contact information) but may include device and usage information, such as your IP
              address, browser and device characteristics, operating system, language preferences,
              referring URLs, device name, country, location, information about how and when you use
              our Services, and other technical information. This information is primarily needed to
              maintain the security and operation of our Services, and for our internal analytics
              and reporting purposes.
            </p>
            <p>
              <strong>Conversational Data and AI Interaction History.</strong> If you use Maple AI
              with memory features enabled, we store your conversation history and information that
              Maple AI has learned about you within your personal encrypted vault. This data is
              encrypted and not accessible to us.
            </p>
            <p>
              <strong>Data from Connected Third-Party Services.</strong> If you connect Maple AI to
              third-party services (such as email providers, cloud storage, health data platforms,
              or productivity tools), Maple AI may access and process data from those services on
              your behalf and in accordance with the permissions you grant. This data is processed
              within our encrypted environment.
            </p>
            <p>
              <strong>API Usage Data.</strong> If you access our services via API, we collect
              information about your API usage, including request volume, timestamps, and error
              logs. If you are a developer using the API to process your end users' data, such data
              is processed on your behalf as a data processor.
            </p>
            <p>
              <strong>Sensitive Data.</strong> You may choose to input sensitive categories of data
              into Maple AI, including health-related information, financial data, or other personal
              information. We process this data solely to provide the Service and in accordance with
              the encryption and security practices described in this notice. We do not use
              sensitive data you input for advertising, profiling, or any purpose other than
              delivering the Service.
            </p>

            <h2>2. How do we process your information?</h2>
            <p>
              We process your personal information for a variety of reasons, depending on how you
              interact with our Services, including:
            </p>
            <ul>
              <li>
                To facilitate account creation and authentication and otherwise manage user
                accounts.
              </li>
              <li>To deliver and facilitate delivery of services to the user.</li>
              <li>To respond to user inquiries and offer support.</li>
              <li>
                To send administrative information to you, including changes to our terms and
                policies.
              </li>
              <li>To fulfill and manage your orders, payments, returns, and exchanges.</li>
              <li>To enable user-to-user communications.</li>
              <li>To request feedback and to contact you about your use of our Services.</li>
              <li>To protect our Services, including fraud monitoring and prevention.</li>
              <li>To identify usage trends so we can improve our Services.</li>
              <li>To determine the effectiveness of our marketing and promotional campaigns.</li>
              <li>To save or protect an individual's vital interest, such as to prevent harm.</li>
              <li>
                To provide personalized AI responses and maintain conversational memory, if you
                enable memory features.
              </li>
              <li>
                To access and interact with third-party services on your behalf when you connect
                them to Maple AI.
              </li>
              <li>
                To process API requests when you or a developer application accesses our services
                via API.
              </li>
            </ul>

            <h2>3. When and with whom do we share your personal information?</h2>
            <p>
              We may share your data with third-party vendors, service providers, contractors, or
              agents ("third parties") who perform services for us or on our behalf and require
              access to such information to do that work. We have contracts in place with our third
              parties, which are designed to help safeguard your personal information. This means
              that they cannot do anything with your personal information unless we have instructed
              them to do it. They will also not share your personal information with any
              organization apart from us. They also commit to protect the data they hold on our
              behalf and to retain it for the period we instruct.
            </p>
            <p>
              For API customers who use our services to process their end users' data, Company acts
              as a data processor on the developer's behalf. The developer, as data controller, is
              responsible for obtaining all necessary consents from their end users and for
              compliance with applicable data protection laws, including GDPR and CCPA.
            </p>

            <h2>4. What is our stance on third-party websites?</h2>
            <p>
              The Services may link to third-party websites, online services, or mobile applications
              and/or contain advertisements from third parties that are not affiliated with us and
              which may link to other websites, services, or applications. Accordingly, we do not
              make any guarantee regarding any such third parties, and we will not be liable for any
              loss or damage caused by the use of such third-party websites, services, or
              applications. The inclusion of a link towards a third-party website, service, or
              application does not imply an endorsement by us. We cannot guarantee the safety and
              privacy of data you provide to any third parties. Any data collected by third parties
              is not covered by this privacy notice. We are not responsible for the content or
              privacy and security practices and policies of any third parties, including other
              websites, services, or applications that may be linked to or from the Services. You
              should review the policies of such third parties and contact them directly to respond
              to your questions.
            </p>
            <p>
              If you connect Maple AI to third-party services such as Gmail, Google Drive, Apple
              Health, Notion, or other productivity and data services, Maple AI will access and
              process data from those services on your behalf and in accordance with the permissions
              you grant. Data retrieved from connected third-party services is processed within our
              encrypted environment and is subject to the same security protections described in
              this notice. We do not share data obtained from your connected third-party services
              with any other third party, except as necessary to provide the Service or as required
              by law. You may revoke Maple AI's access to any connected third-party service at any
              time through your account settings. Please note that third-party services have their
              own privacy policies, and we are not responsible for their data practices. We
              encourage you to review the privacy policies of any services you connect to Maple AI.
            </p>

            <h2>5. Do we use cookies and other tracking technologies?</h2>
            <p>
              We may use cookies and similar tracking technologies (like web beacons and pixels) to
              access or store information.
            </p>

            <h2>6. How do we handle your social logins?</h2>
            <p>
              Our Services offer you the ability to register and log in using your third-party
              social media account details. Where you choose to do this, we will receive certain
              profile information about you from your social media provider. The profile information
              we receive may vary depending on the social media provider concerned, but will often
              include your name, email address, friends list, and profile picture, as well as other
              information you choose to make public on such a social media platform.
            </p>
            <p>
              We will use the information we receive only for the purposes that are described in
              this privacy notice or that are otherwise made clear to you on the relevant Services.
              Please note that we do not control, and are not responsible for, other uses of your
              personal information by your third-party social media provider. We recommend that you
              review their privacy notice to understand how they collect, use and share your
              personal information, and how you can set your privacy preferences on their sites and
              apps.
            </p>

            <h2>7. How do we keep your information safe?</h2>
            <p>
              We have implemented appropriate and reasonable technical and organizational security
              measures designed to protect the security of any personal information we process.
              However, despite our safeguards and efforts to secure your information, no electronic
              transmission over the Internet or information storage technology can be guaranteed to
              be 100% secure, so we cannot promise or guarantee that hackers, cybercriminals, or
              other unauthorized third parties will not be able to defeat our security and
              improperly collect, access, steal, or modify your information. Although we will do our
              best to protect your personal information, transmission of personal information to and
              from our Services is at your own risk. You should only access the Services within a
              secure environment.
            </p>
            <p>
              By default, Maple AI operates with minimal data retention, consistent with our
              commitment to privacy. Conversational data processed without memory features enabled
              is not retained after your session ends.
            </p>
            <p>
              If you enable memory features, Maple AI retains your conversational data and learned
              preferences within your personal encrypted vault for as long as the memory feature
              remains enabled. You have the right to:
            </p>
            <ul>
              <li>View your stored memory data through your account settings;</li>
              <li>Export your data in a portable format upon request;</li>
              <li>
                Delete specific memory data or all stored memory data at any time through your
                account settings;
              </li>
              <li>
                Disable memory features, which will stop new data from being retained (previously
                stored data must be separately deleted).
              </li>
            </ul>
            <p>
              Deleting your account will result in the deletion of your encrypted conversational
              data, subject to any legal retention obligations.
            </p>
            <p>
              For all users, we retain account metadata (such as your name, email, and payment
              information) for as long as your account remains active and for a reasonable period
              thereafter to comply with legal obligations, resolve disputes, and enforce our
              agreements.
            </p>
            <p>
              To exercise your data rights, including requests for access, deletion, or data
              portability, please contact us at{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>

            <h2>8. Do we collect information from minors?</h2>
            <p>
              We do not knowingly solicit data from or market to children under 18 years of age. By
              using the Services, you represent that you are at least 18 or that you are the parent
              or guardian of such a minor and consent to such minor dependent's use of the Services.
              If we learn that personal information from users less than 18 years of age has been
              collected, we will deactivate the account and take reasonable measures to promptly
              delete such data from our records. If you become aware of any data we may have
              collected from children under age 18, please contact us at{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>

            <h2>9. Controls for do-not-track features</h2>
            <p>
              Most web browsers and some mobile operating systems and mobile applications include a
              Do-Not-Track ("DNT") feature or setting you can activate to signal your privacy
              preference not to have data about your online browsing activities monitored and
              collected. At this stage no uniform technology standard for recognizing and
              implementing DNT signals has been finalized. As such, we do not currently respond to
              DNT browser signals or any other mechanism that automatically communicates your choice
              not to be tracked online. If a standard for online tracking is adopted that we must
              follow in the future, we will inform you about that practice in a revised version of
              this privacy notice.
            </p>

            <h2>10. Do we make updates to this notice?</h2>
            <p>
              We may update this privacy notice from time to time. The updated version will be
              indicated by an updated "Revised" date and the updated version will be effective as
              soon as it is accessible. If we make material changes to this privacy notice, we may
              notify you either by prominently posting a notice of such changes or by directly
              sending you a notification. We encourage you to review this privacy notice frequently
              to be informed of how we are protecting your information.
            </p>

            <h2>11. How can you contact us about this notice?</h2>
            <p>
              If you have questions or comments about this notice, you may contact us at{" "}
              <a href="mailto:support@trymaple.ai">support@trymaple.ai</a>.
            </p>
          </article>
        </div>
      </FullPageMain>
    </>
  );
}
